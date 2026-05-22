# drevo — Kubernetes manifests

Phase 8 task `00049` (base) + `00050` (overlays). Plain
`kubectl`-applyable manifests that deploy
[`ghcr.io/ice1x/drevo`](../Dockerfile) as a single-replica HTTP service
backed by a ReadWriteOnce PersistentVolumeClaim.

## What is here

| File                              | Kind                       | Purpose                                                                              |
|-----------------------------------|----------------------------|--------------------------------------------------------------------------------------|
| `kustomization.yaml`              | Kustomize wrapper          | Forwards to `./base/` so the historical `kubectl apply -k k8s/` keeps working        |
| `base/deployment.yaml`            | `apps/v1` Deployment       | Runs `drevo-server`, wires `/health` + `/ready` probes, mounts `/data`               |
| `base/service.yaml`               | `v1` Service (ClusterIP)   | Fronts the pod on port 8080                                                          |
| `base/pvc.yaml`                   | `v1` PersistentVolumeClaim | 1Gi ReadWriteOnce volume; bumps via in-place expansion on supporting storage classes |
| `base/kustomization.yaml`         | Kustomize base             | Lets `kubectl apply -k k8s/base/` apply all three at once                            |
| `overlays/dev/{kustomization,deployment-patch,pvc-patch}.yaml`  | Kustomize overlay | Dev-cluster namespace + smaller resources, 1Gi PVC, debug logging (task `00050`)     |
| `overlays/prod/{kustomization,deployment-patch,pvc-patch}.yaml` | Kustomize overlay | Prod-cluster namespace + larger resources, 20Gi PVC, `imagePullPolicy: Always` (task `00050`) |

## Quick start

```bash
# Plain kubectl
kubectl apply -f k8s/base/pvc.yaml
kubectl apply -f k8s/base/deployment.yaml
kubectl apply -f k8s/base/service.yaml

# Or with kustomize (bundled in modern kubectl) — wrapper forwards to ./base/
kubectl apply -k k8s/
# Equivalent direct form
kubectl apply -k k8s/base/

# Or with the per-env overlay
kubectl apply -k k8s/overlays/dev/   # → namespace drevo-dev
kubectl apply -k k8s/overlays/prod/  # → namespace drevo-prod

# Watch the rollout
kubectl rollout status deployment/drevo

# Port-forward and run a smoke test
kubectl port-forward svc/drevo 8080:8080 &
curl -sf http://127.0.0.1:8080/health
curl -sf http://127.0.0.1:8080/status
```

## Design notes

* **Single replica.** drevo is a single-writer redb store. Two writers
  would corrupt the database file — MVCC arrives in Phase 13
  (task `00080`). The Deployment locks `replicas: 1` and
  `strategy.type: Recreate` so the new pod waits for the old one to
  release the RWO PVC before it starts.
* **Probes mirror the HTTP API contract.** `livenessProbe` hits
  `/health` (cheap, DB-independent — task `00042`); `readinessProbe`
  hits `/ready` (probes redb — task `00048`). Both flip to 503 during
  SIGTERM drain so the Kubernetes Endpoints controller withdraws
  traffic from the pod before `SIGKILL`.
* **Non-root pod.** The container image already drops to the `drevo`
  system user; the pod-level `securityContext` (`runAsNonRoot: true`,
  `runAsUser: 999`, `fsGroup: 999`) keeps that contract visible at the
  manifest layer and chowns the mounted PVC to the right gid.
* **`terminationGracePeriodSeconds: 30`.** Gives the HTTP server time
  to finish in-flight requests before SIGKILL. Bump it for slow,
  long-running queries; do not lower it without re-checking that the
  drain loop in `src/server.rs` completes inside the new budget.
* **ClusterIP only.** drevo is a backing store, not a public-facing
  service. Front it with an Ingress / Gateway you control (nginx,
  traefik, ALB, GKE Gateway, …) — that decision is cluster-specific
  and intentionally not baked into this base.

## Storage sizing

The default PVC requests `1Gi`. Production deployments will outgrow
that quickly once FTS indices are populated. Either:

* apply the `overlays/prod/` overlay (requests 20Gi out of the box), or
* edit `base/pvc.yaml` before applying, or
* live-expand on storage classes with `allowVolumeExpansion: true`:
  ```bash
  kubectl patch pvc drevo-data -p '{"spec":{"resources":{"requests":{"storage":"20Gi"}}}}'
  ```

## Kustomize overlays

Task `00050`. `k8s/overlays/<env>/` wraps the base above with
environment-specific patches. Each overlay:

* sets a distinct `namespace:` (`drevo-dev`, `drevo-prod`) so the same
  cluster can host both side by side;
* overrides the image tag via the kustomize `images:` block (dev →
  `v0.1.0-dev`, prod → `v0.1.0`) — the base image name stays unchanged
  so the rewrite is a one-line bump per release;
* ships strategic-merge patches that **only** touch container resources
  and PVC storage size. The single-writer redb invariants
  (`replicas: 1`, `strategy.type: Recreate`, `ReadWriteOnce`,
  `runAsNonRoot: true`) live in the base and are explicitly forbidden
  from being relaxed by an overlay (`tests/k8s_overlays_tests.rs`).

| Overlay | Namespace   | Image tag       | CPU req → lim    | Memory req → lim   | PVC size |
|---------|-------------|-----------------|------------------|---------------------|----------|
| `dev`   | `drevo-dev` | `v0.1.0-dev`    | 25m → 250m       | 32Mi → 256Mi        | 1Gi      |
| `prod`  | `drevo-prod`| `v0.1.0`        | 200m → 2000m     | 256Mi → 2Gi         | 20Gi     |

`overlays/dev/` also injects `RUST_LOG=debug` into the container env
for development convenience. `overlays/prod/` flips
`imagePullPolicy: Always` so a `kubectl rollout restart` re-pulls the
pinned tag from ghcr.io rather than reusing a stale node-cached image.

Apply with `kubectl apply -k k8s/overlays/<env>/`, or render to YAML
without applying via `kubectl kustomize k8s/overlays/<env>/`. CI runs
the render step for every overlay so a kustomize-syntax slip or a
broken `../../` base reference fails the build.

## What this does not include

* **Ingress / Gateway.** See above — pick the controller that matches
  your cluster.
* **Helm chart.** The kustomize overlays above cover the per-env
  patching that a Helm chart would otherwise wrap; a chart can be
  layered on top later if templating with values becomes preferable
  to strategic-merge patches.
* **Image publish pipeline.** Tracked as task `00051`. Until that
  lands, `image: ghcr.io/ice1x/drevo:v0.1.0` resolves to whatever a
  developer pushed manually; until it is automated, build locally and
  `docker push` to a private registry, then edit the `image:` line.
* **Container persistence integration test.** Tracked as task `00052`.

## Related contracts

* `Dockerfile` — exposes 8080, defaults `DREVO_DATA_DIR=/data`, runs
  as the `drevo` user (UID 999).
* `docker-compose.yml` — the same shape for local development.
* `tests/k8s_manifests_tests.rs` — verifies the base manifests under
  `k8s/base/` keep the contracts above (probe paths, env vars, RWO
  PVC, ClusterIP port, image reference) and that the top-level
  `k8s/kustomization.yaml` wrapper forwards to `./base/`.
* `tests/k8s_overlays_tests.rs` — verifies each overlay under
  `k8s/overlays/<env>/` carries the env-specific kustomization,
  deployment patch, and PVC patch; locks the namespace + image tag +
  resource ordering invariants (prod ≥ dev) and forbids overlays from
  bumping `replicas` above 1, relaxing `runAsNonRoot`, or relaxing
  `ReadWriteOnce` to `ReadWriteMany`.
* Both suites parse the manifests as text and require no Kubernetes
  runtime.

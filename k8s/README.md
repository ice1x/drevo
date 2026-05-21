# drevo — Kubernetes manifests

Phase 8 task `00049`. Plain `kubectl`-applyable manifests that deploy
[`ghcr.io/ice1x/drevo`](../Dockerfile) as a single-replica HTTP service
backed by a ReadWriteOnce PersistentVolumeClaim.

## What is here

| File                  | Kind                    | Purpose                                                                       |
|-----------------------|-------------------------|-------------------------------------------------------------------------------|
| `deployment.yaml`     | `apps/v1` Deployment    | Runs `drevo-server`, wires `/health` + `/ready` probes, mounts `/data`        |
| `service.yaml`        | `v1` Service (ClusterIP)| Fronts the pod on port 8080                                                   |
| `pvc.yaml`            | `v1` PersistentVolumeClaim | 1Gi ReadWriteOnce volume; bumps via in-place expansion on supporting storage classes |
| `kustomization.yaml`  | Kustomize base          | Lets `kubectl apply -k k8s/` apply all three at once                          |

## Quick start

```bash
# Plain kubectl
kubectl apply -f k8s/pvc.yaml
kubectl apply -f k8s/deployment.yaml
kubectl apply -f k8s/service.yaml

# Or with kustomize (bundled in modern kubectl)
kubectl apply -k k8s/

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

* edit `pvc.yaml` before applying, or
* live-expand on storage classes with `allowVolumeExpansion: true`:
  ```bash
  kubectl patch pvc drevo-data -p '{"spec":{"resources":{"requests":{"storage":"20Gi"}}}}'
  ```

## What this does not include

* **Ingress / Gateway.** See above — pick the controller that matches
  your cluster.
* **Helm chart / overlays.** Tracked as task `00050`. The
  `kustomization.yaml` here is the chart's `templates/` equivalent and
  can be wrapped by either tool later.
* **Image publish pipeline.** Tracked as task `00051`. Until that
  lands, `image: ghcr.io/ice1x/drevo:v0.1.0` resolves to whatever a
  developer pushed manually; until it is automated, build locally and
  `docker push` to a private registry, then edit the `image:` line.
* **Container persistence integration test.** Tracked as task `00052`.

## Related contracts

* `Dockerfile` — exposes 8080, defaults `DREVO_DATA_DIR=/data`, runs
  as the `drevo` user (UID 999).
* `docker-compose.yml` — the same shape for local development.
* `tests/k8s_manifests_tests.rs` — verifies the manifests in this
  directory keep the contracts above (probe paths, env vars, RWO PVC,
  ClusterIP port, image reference). The tests parse the manifests as
  text and require no Kubernetes runtime.

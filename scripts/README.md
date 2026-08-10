# `scripts/`

Operational helpers for running drevo outside Kubernetes.

| Script | What it does |
| --- | --- |
| [`release.sh`](release.sh) | Compute the next `vX.Y.Z`, create the annotated git tag, and push it — the CI [`docker-publish`](../.github/workflows/docker-publish.yml) workflow then builds and publishes the image. |
| [`drevo-restart.sh`](drevo-restart.sh) | One-command bare-`docker run` (re)start for single-host setups that don't use Compose. Carries `--restart unless-stopped`. |
| [`drevo-watchdog.sh`](drevo-watchdog.sh) | Recreate the local container if it is missing or not running — self-heals even an accidental `docker rm -f` that a restart policy cannot cover. |
| [`watchdog/`](watchdog/) | launchd (macOS) and systemd (Linux) templates that run the watchdog on a 30s schedule. |

## Keeping the local container alive

Two layers, because they cover different failures:

1. **Docker restart policy** — `restart: unless-stopped` in
   [`docker-compose.yml`](../docker-compose.yml) (and `--restart unless-stopped`
   in `drevo-restart.sh`) relaunches the container after a **crash, OOM-kill, or
   Docker/host reboot**. It does **not** act on an intentional `stop`, nor can it
   resurrect a **removed** container — `docker rm -f` deletes the container
   object, leaving nothing to restart.

2. **Watchdog** — `drevo-watchdog.sh`, run every 30s by launchd/systemd, closes
   that gap: if the container is absent or not `running`, it recreates it (via
   `docker compose up -d` by default, or your `DREVO_UP_CMD`). This is the layer
   that survives an accidental `docker rm -f drevo` from another project's
   teardown reusing the same name.

Install the schedule from a template in [`watchdog/`](watchdog/) (edit the
absolute checkout path first). To stop the container on purpose without the
watchdog fighting you, `touch ~/.drevo-watchdog.disabled` (remove it to resume).

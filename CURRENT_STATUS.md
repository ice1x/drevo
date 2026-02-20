# GrapeVine — Current Status

> This file is updated after every working session.
> Pass it as context to the AI assistant at the start of each new session.

## Current Phase

Phase 1: Foundation — Storage Engine

## Last Completed Task

Project has not started yet. Next task: `0001`.

## Completed

_Nothing yet._

## In Progress

_Nothing yet._

## Blockers / Questions

_None yet._

## Known Bugs

_None yet._

## Decisions Made Along the Way

> Record architectural decisions made during development that differ from the original plan.

- **2026-02-20**: HTTP API (axum) moved from Phase 8 (post-MVP) to Phase 6.5 (MVP). Rationale: Docker deployment requires a network API for programmatic access from Python client. CLI REPL alone is insufficient for inter-process communication.
- **2026-02-20**: Docker support (Dockerfile, docker-compose) added as Phase 6.5 tasks (`0072`-`0076`). Rationale: the primary deployment target is a Docker container accessible from Python.
- **2026-02-20**: Python client specification (`PYTHON_CLIENT_SPEC.md`) created. The Python client will be developed in a separate repository. The spec serves as the API contract and must be reviewed/updated after every task.
- **2026-02-20**: Mandatory spec review protocol added to `CONVENTIONS.md` and `AGENT_INSTRUCTIONS.md` — after every task/subtask, the agent must check and update `PYTHON_CLIENT_SPEC.md`.

## Test Status

```
cargo test: N/A
cargo clippy: N/A
```

## Next Steps

1. `0001` — Define `StorageBackend` trait
2. `0002` — Define error types
3. `0003` — Implement `MemoryBackend`

---

## Update Template

After each session, update this file:

```markdown
## Current Phase
Phase N: ...

## Last Completed Task
`XXXX` — description

## Completed
- `XXXX` description — done
- `YYYY` description — done

## In Progress
- `ZZZZ` description — what remains

## Blockers / Questions
- Question or problem

## Decisions Made Along the Way
- Decided to use X instead of Y because Z

## Test Status
cargo test: XX passed, 0 failed
cargo clippy: 0 warnings

## Next Steps
1. `XXXX` — next task
2. `YYYY` — and another
```

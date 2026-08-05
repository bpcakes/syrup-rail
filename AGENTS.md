# Repository Guidelines

<!-- BEGIN JIG MANAGED BLOCK -->
This repository uses the shared `jig.sh` workflow. Keep repo-local business rules and ownership guidance in crate-level guides; keep generic agent workflow and repo policy here.

## Start Here

- Use this file for repo-wide defaults.
- Open [agent-map.md](./agent-map.md) before backend work.
- Read the nearest crate-level `AGENTS.md` before changing a crate when one exists.
- Use `.agent/PLANS.md` when writing an ExecPlan for a complex feature or refactor.
- Use `scripts/jig` for the typed repo contract and `scripts/jig mcp` for MCP clients.
- On a fresh machine, run `scripts/jig doctor`; follow its next step, including `scripts/jig agent bootstrap` when Jig Codex skills are missing.
- For substantial work, use `scripts/jig work start`, `scripts/jig work check`, `scripts/jig work evidence`, `scripts/jig work gates`, and `scripts/jig work finish` to keep plans, receipts, and required gates connected.
- Treat `.agent/state/*.jsonl` as append-only repo memory.

## Compatibility And Cutovers

- Prefer direct cutovers only for internal code-only changes that can ship in one coordinated deploy.
- Preserve compatibility or stage rollouts for persisted database state, queued job types, public API contracts, bookmarked routes, webhook boundaries, or source-of-truth moves that can straddle deploys.

- Never overwrite an existing database migration; add a new forward-only migration instead.


## Backend Defaults

- Treat `crates` as Rust crate roots.
- Add crate-level `AGENTS.md` files when a crate has meaningful ownership, entrypoint, or invariant guidance that should travel with that crate.

- SQL migrations live under `migrations`.
- SQLx metadata is committed in `.sqlx`.

- Keep transport logic thin and business logic in the owning crate.

- Keep transaction boundaries explicit and deterministic.


## Frontend Defaults

No web apps are configured in `.jig.toml`.


## Preferred Commands

- `scripts/jig bootstrap`
- `scripts/jig doctor`
- `scripts/jig dev`
- `scripts/jig check test`
- `scripts/jig check fmt`
- `scripts/jig check clippy`
- `scripts/jig work status`
- `scripts/jig work evidence`


- `scripts/jig check sqlx`

- `scripts/jig migration-add NAME`

- `scripts/jig check contract`

## Done Means

- Run the relevant local verification for the area you changed.
- For backend changes, finish with `scripts/jig check test`.


- For SQLx or migration changes, run `scripts/jig check sqlx`.


- Review the generated diff for stale docs, policy drift, or missing dependent updates.

## Crate Guide Conventions

When a backend crate has a crate-level `AGENTS.md`, use these sections:

- `## Purpose`
- `## Key entrypoints`
- `## Edit here for X`
- `## Invariants`
- `## Common commands`
<!-- END JIG MANAGED BLOCK -->
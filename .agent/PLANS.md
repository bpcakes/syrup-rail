# Codex Execution Plans (ExecPlans)

This document defines the contract for a self-contained execution plan that another engineer or agent can implement without prior context.

## Required Properties

- Every ExecPlan must be self-contained.
- Every ExecPlan must be a living document.
- Every ExecPlan must let a novice implement the work end to end.
- Every ExecPlan must describe observable outcomes, not just code edits.

## Required Sections

Every ExecPlan must contain these sections and keep them current:

- `Progress`
- `Surprises & Discoveries`
- `Decision Log`
- `Outcomes & Retrospective`

## Writing Rules

- Write for a reader who has only the current worktree and the ExecPlan.
- Define non-obvious terms in plain language.
- Name exact paths, modules, commands, and expected outcomes.
- Include commands to run, what success looks like, and how to recover from partial failure.
- Treat durable state and compatibility-sensitive changes explicitly.
- Follow `docs/security/threat-model.md` for security- or privacy-sensitive work. Link to that baseline and document only feature-specific classifications or explicitly reviewed extensions.
- Before adding encryption, hashing/HMAC, signatures, pervasive zeroization, tamper-evident storage, a new secret, or a privileged security boundary, answer the threat model's required design test. Do not add controls that address only an out-of-scope threat or duplicate an existing control.

## Suggested Skeleton

Use this shape:

1. Title and purpose
2. `Progress`
3. `Surprises & Discoveries`
4. `Decision Log`
5. `Outcomes & Retrospective`
6. Context and orientation
7. Plan of work
8. Concrete steps
9. Validation and acceptance
10. Idempotence and recovery
11. Interfaces and dependencies

## Maintenance Rule

When revising an ExecPlan, update every affected section so the file remains restartable from scratch.

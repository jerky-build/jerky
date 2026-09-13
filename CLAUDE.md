# jerky

## Agent skills

### Issue tracker

Issues live as GitHub issues in `jerky-build/jerky`, managed with the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical triage roles, using the default label strings. See `docs/agents/triage-labels.md`.

### Engineering invariants

Standing implementation rules, and how to check a diff against them. See `docs/agents/invariants.md`.

### Design docs

Specs live in `docs/specs/`, research notes in `docs/research/`, both named `YYYY-MM-DD-slug.md`.

Implementation plans have **no file form**. A spec is split into GitHub issues, one per task, each carrying the tests to write first, the implementation notes, and the spec sections it answers to, wired with native issue dependencies so a blocked task reads as blocked. Decided in cb03538 — plan documents had no reader, since no skill here opens one.

### Domain docs

Single-context: `CONTEXT.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.

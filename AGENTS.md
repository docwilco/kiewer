## Agent skills

### Issue tracker

GitHub Issues in `docwilco/kiewer` via `gh`, inferred from the `origin` remote. See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.

## Product

kiewer: a site for uploading and sharing KiCad designs, rendered in the browser. Before designing or building any feature, read `docs/v1-scope.md` (every v1 decision, by area) and the ADRs in `docs/adr/`; the hand-off that started it is `viewer-drop/README.md`.

## Development

`scripts/checks.sh` is the single source for checks; the pre-commit hook in
`.githooks/` and CI both run it. The hook checks a snapshot of the index, not
the working tree. Rust conventions and lint triage come from the
`rust-conventions` skill: invoke it before editing Rust here.

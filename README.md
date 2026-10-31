# kiewer

A site for uploading and sharing KiCad designs, rendered in the browser. The design is in `docs/v1-scope.md` and `docs/adr/`; the vocabulary in `GLOSSARY.md`.

## Development

One-time, after cloning: `git config core.hooksPath .githooks`. The hook runs
`scripts/checks.sh` (fmt, clippy, test, docs) before every commit against a
snapshot of the index, so unstaged edits never reach the checks and the
working tree stays usable while they run; it needs `rsync` and `cargo-sweep`.
CI runs the same script. Run a single stage with `scripts/checks.sh clippy`.

Clippy is pedantic with no blanket allows: every finding is fixed or carries a
site-level `#[expect(..., reason = "...")]` naming the invariant that makes it safe.

## Infrastructure

GCP and Fastly are Terraform in `infra/`, applied by hand from a workstation;
CI only deploys Cloud Run revisions with the identity it defines there.
`infra/README.md` is the procedure: bootstrapping the state bucket, the apply,
setting the three repository variables from the outputs, and what a plan after
a CI deploy should show.

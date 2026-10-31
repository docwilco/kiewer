# kiewer

A site for uploading and sharing KiCad designs, rendered in the browser. The design is in `docs/v1-scope.md` and `docs/adr/`; the vocabulary in `GLOSSARY.md`.

## Development

One-time, after cloning: `git config core.hooksPath .githooks`, then
`rustup target add wasm32-unknown-unknown` for the viewer's browser build.
The hook runs `scripts/checks.sh` (the stages are listed in its usage line)
before every commit against a snapshot of the index, so unstaged edits never
reach the checks and the working tree stays usable while they run; it needs
`rsync` and `cargo-sweep`. CI runs the same script. Run a single stage with
`scripts/checks.sh clippy`.

Running the server: `docker compose up -d` starts Postgres (`compose.yaml`),
`cp .env.example .env` and export it (`set -a; . ./.env; set +a`), then
`cargo run`. The first line on stdout is the build line; the page is at
http://localhost:8080/ and `/healthz` says `ok` once the database answers.
`kiewer_lib::config` is the list of variables the server reads and their
defaults; the blob store writes under `blobs/` (git-ignored) until the GCS
store exists. Ctrl-c or SIGTERM stops it after the requests in flight.

The tests that need Postgres (`kiewer_lib/tests/db.rs`) run when
`DATABASE_URL` is exported and skip when it is not, in the hook as in a
shell; CI sets it against a service container that matches `compose.yaml`.
The schema is `migrations/` (its README has the naming rule); it is empty
until the first table.

Clippy is pedantic with no blanket allows: every finding is fixed or carries a
site-level `#[expect(..., reason = "...")]` naming the invariant that makes it safe.

## Infrastructure

GCP and Fastly are Terraform in `infra/`, applied by hand from a workstation;
CI only deploys Cloud Run revisions with the identity it defines there.
`infra/README.md` is the procedure: bootstrapping the state bucket, the apply,
setting the three repository variables from the outputs, and what a plan after
a CI deploy should show.

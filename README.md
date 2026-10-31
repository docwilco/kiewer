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
defaults; the blob store writes under `blobs/` (git-ignored) unless
`BLOB_BUCKET` names a GCS bucket, which makes it the GCS store (with
`KIEWER_GCS_EMULATOR` beside it, on the emulator below). Ctrl-c or SIGTERM
stops it after the requests in flight.

The tests that need Postgres (`kiewer_lib/tests/db.rs`, `accounts.rs`,
`sign_in.rs`) run when `DATABASE_URL` is exported and skip when it is not,
in the hook as in a shell; CI sets it against a service container that
matches `compose.yaml`. The schema is `migrations/` (its README has the
naming rule).

Sign-in needs a GitHub OAuth app of your own, one per environment
(docs/v1-scope.md, Q57): at https://github.com/settings/developers, New
OAuth App, any name, homepage `http://localhost:8080`, callback URL
`http://localhost:8080/auth/github/callback` (the path is fixed; the
origin is `PUBLIC_URL`, which defaults to localhost on `PORT`). Generate a
client secret and put both in `.env` as `OAUTH_GITHUB_CLIENT_ID` and
`OAUTH_GITHUB_CLIENT_SECRET`, the names `infra/` hands Cloud Run too.
Without them the server runs with sign-in off and `/auth/github` answers
404. Cookies are `Secure` under an `https` `PUBLIC_URL`, so a dev box over
`http` works in every browser. The sign-in tests use a fake GitHub on
loopback and need no app. Production and staging get their registrations
through Secret Manager, not `.env`.

The GCS store test (`kiewer_lib/tests/gcs_blob_store.rs`) runs the same way
when `KIEWER_GCS_EMULATOR` names a
[fake-gcs-server](https://github.com/fsouza/fake-gcs-server) and skips when
it is not set. On a dev box: the release binary for your platform (v1.56.1,
the version CI pins), then
`fake-gcs-server -scheme http -host 127.0.0.1 -port 4443 -backend memory -public-host 127.0.0.1`
and `export KIEWER_GCS_EMULATOR=http://127.0.0.1:4443`.

Clippy is pedantic with no blanket allows: every finding is fixed or carries a
site-level `#[expect(..., reason = "...")]` naming the invariant that makes it safe.

### Exit codes

The executable's non-zero exits, each a kind of `kiewer_lib::error::Error`
(`Exit` there renders this table; a test keeps the two equal). 0 is success;
1, 2, 101 and 126 up are never assigned.

| Code | Kind | Meaning |
|---|---|---|
| 3 | Io | I/O, including the blob store's files |
| 4 | Blob | a blob that is corrupt or a hash that does not parse |
| 5 | Config | the environment is missing or malformed |
| 6 | Db | Postgres, including migrations |
| 7 | Template | a page did not render |
| 8 | Failed | a failure with no better kind |
| 9 | Upstream | a sign-in provider or another service over HTTP |

## Infrastructure

GCP and Fastly are Terraform in `infra/`, applied by hand from a workstation;
CI only deploys Cloud Run revisions with the identity it defines there.
`infra/README.md` is the procedure: bootstrapping the state bucket, the apply,
setting the three repository variables from the outputs, and what a plan after
a CI deploy should show.

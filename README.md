# kiewer

A site for uploading and sharing KiCad designs, rendered in the browser. The design is in `docs/v1-scope.md` and `docs/adr/`; the vocabulary in `GLOSSARY.md`.

## Development

One-time, after cloning: `git config core.hooksPath .githooks`, then for
the viewer's browser build `rustup target add wasm32-unknown-unknown` and
`cargo install wasm-bindgen-cli --version "$(scripts/wasm-bindgen-version.sh)" --locked`,
and for the sqlx stage
`cargo install sqlx-cli --version "$(scripts/sqlx-version.sh)" --locked --no-default-features --features postgres,rustls`
(each the version Cargo.lock names; CI installs the same way). The hook runs
`scripts/checks.sh` (the stages are listed in its usage line) before every
commit against a snapshot of the index, so unstaged edits never reach the
checks and the working tree stays usable while they run; it needs `rsync`,
`jq` and `cargo-sweep`. CI runs the same script. Run a single stage with
`scripts/checks.sh clippy`. The same setting enables a post-merge hook:
after a merge or pull, fast-forwards included, it removes what the
checkout's target has not used in three days (`.githooks/sweep.sh`, which
the pre-commit hook runs too), and prints a line when it skips.

Running the server: `docker compose up -d` starts Postgres (`compose.yaml`),
`cp .env.example .env` and export it (`set -a; . ./.env; set +a`),
`scripts/build-viewer.sh` once (and again after a change to `kiewer_viewer`
or `kicad_core`: it writes the browser viewer to `kiewer_viewer/dist`, which
`cargo run` embeds; without it the server runs and the viewer page says the
viewer is not built in), then `cargo run`. The first line on stdout is the
build line; the page is at http://localhost:8090/, the viewer with the
hand-off board at http://localhost:8090/viewer, its schematic at
http://localhost:8090/schematic, the upload form at
http://localhost:8090/upload once signed in, and `/healthz` says `ok` once
the database answers.
`kiewer_lib::config` is the list of variables the server reads and their
defaults; the blob store writes under `blobs/` (git-ignored) unless
`BLOB_BUCKET` names a GCS bucket, which makes it the GCS store (with
`KIEWER_GCS_EMULATOR` beside it, on the emulator below). `FASTLY_SERVICE_ID`
and `FASTLY_API_TOKEN` stay unset here: nothing is in front of the server,
so a write purges nothing. `SURROGATE_KEY_PREFIX` goes in front of every
cache key the server names and purges; `infra/` sets `staging-` on
kiewer-staging, which shares production's Fastly service, and a dev box
leaves it unset. The log is text
on a terminal and JSON lines anywhere else (Cloud Run reads those into
`jsonPayload`; the renderer beacon's metric in `infra/logging.tf` counts
them); `LOG_FORMAT=json` or `text` overrides. Ctrl-c or SIGTERM stops it
after the requests in flight.

The tests that need Postgres (every user of `kiewer_test_support::db`)
run when `DATABASE_URL` is exported; CI sets it against a service
container that matches `compose.yaml`. Without it such a test returns
early and counts as passed, so `scripts/checks.sh test` fails without
it, unless `KIEWER_SKIP_DB_TESTS=1` is set: then they skip, and a loud
line names them before the stages and again after the last. CI refuses
the opt-out. The hook reads the variable from the shell that runs
`git commit`, never from `.env`: each worktree needs a database of its
own, since on a shared one sqlx refuses a worktree that lacks a
migration another applied (`VersionMissing`). The collector's tests
create and use a database of their own beside it, the URL's name plus
`_collector`, so the role needs CREATEDB (compose's and CI's `kiewer` is
the image's superuser). The schema is `migrations/` (its README has the
naming rule).

Every statement in `kiewer_lib` is a `sqlx::query!`, `query_as!` or
`query_scalar!` macro, checked at compile time against the offline data
committed in `.sqlx/`; statement text several statements share is spliced
in by `kiewer_lib/src/sql.rs`. `scripts/checks.sh` and
`scripts/release-build.sh` export `SQLX_OFFLINE=true`, so a build reads
that data and never a database; `.env.example` sets it too, since the
macros read a `.env` here or in any directory above. After a change to a
statement or a migration, `scripts/sqlx-prepare.sh` rewrites `.sqlx/`
against a scratch database beside `DATABASE_URL`'s (its name plus
`_sqlx`, migrated from `migrations/`), and `git add -A .sqlx` stages the
new and the deleted files with the change. The sqlx stage runs it with
`--check` and fails while the data is stale. While a statement is in
flux, build with `SQLX_OFFLINE=false` and `DATABASE_URL` on that scratch
database, which the last prepare left migrated.

Sign-in needs an OAuth app of your own at each provider you want, one per
environment (docs/v1-scope.md, Q57). The callback URL to register is
`PUBLIC_URL` (default `http://localhost:8090`, localhost on `PORT`) plus
`/auth/<provider>/callback`; the client id and secret go in `.env` as
`OAUTH_<PROVIDER>_CLIENT_ID` and `OAUTH_<PROVIDER>_CLIENT_SECRET`, the
names `infra/` hands Cloud Run too. A provider with neither variable is
left off the front page and its `/auth/<provider>` answers 404; one
variable without the other fails startup naming the missing one.

- GitHub: https://github.com/settings/developers, New OAuth App, any name,
  homepage `http://localhost:8090`, callback
  `http://localhost:8090/auth/github/callback`; generate a client secret.
  Variables `OAUTH_GITHUB_*`.
- Discord: https://discord.com/developers/applications, New Application,
  OAuth2, add the redirect `http://localhost:8090/auth/discord/callback`;
  the client id and secret are on that page. The server asks the
  `identify` scope only. Variables `OAUTH_DISCORD_*`.
- Google: https://console.cloud.google.com/apis/credentials, Create
  credentials, OAuth client ID, Web application, authorised redirect URI
  `http://localhost:8090/auth/google/callback`; the consent screen needs
  the `profile` scope only and, while it is in testing, your account as
  a test user. Variables `OAUTH_GOOGLE_*`.

Cookies are `Secure` under an `https` `PUBLIC_URL`, so a dev box over
`http` works in every browser. The sign-in tests use a fake of each
provider on loopback and need no app. Production and staging get their
registrations through Secret Manager, not `.env`.

The GCS store test (`kiewer_lib/tests/it/gcs_blob_store.rs`) runs when
`KIEWER_GCS_EMULATOR` names a
[fake-gcs-server](https://github.com/fsouza/fake-gcs-server), under the
same rule as Postgres: without it `scripts/checks.sh test` fails, unless
`KIEWER_SKIP_GCS_TESTS=1` is set, which CI refuses. On a dev box: the
release binary for your platform (v1.56.1, the version CI pins), then
`fake-gcs-server -scheme http -host 127.0.0.1 -port 8092 -backend memory -public-host 127.0.0.1`
and `export KIEWER_GCS_EMULATOR=http://127.0.0.1:8092`.

The golden-image test (`kiewer_viewer/tests/golden.rs`) renders the
hand-off board headless through wgpu on Vulkan and compares each view
with `viewer-drop/exports/png`; its tolerance is in its module doc. It
needs a Vulkan driver: on a box without a GPU that is Mesa's lavapipe,
`apt install mesa-vulkan-drivers`, the package CI and the Dockerfile
install. Without one it skips with a line saying so; on CI it fails
instead. `cargo run --bin kiewer_render_dev -- viewer-drop/board/midi8knob.kicad_pcb <dir>`
writes the same PNGs for a look.

The browser stage (`scripts/checks.sh browser`, in `all`) loads the board
page in Chrome for Testing's `chrome-headless-shell`, which draws WebGL2 on
SwiftShader with no GPU (`kiewer_lib/tests/it/browser.rs`). On a dev box:
`sudo apt install $(scripts/browser.sh packages)` for the libraries it
links against (Ubuntu 24.04 names), then `scripts/browser.sh install`,
which downloads the version the script pins, checks its SHA-256 and
unpacks it under `~/.cache/kiewer` (it needs `curl` and `unzip`); CI
installs the same way. Without it the stage skips with a line saying so;
on CI it fails instead. The Dockerfile installs neither, though programs
the tests run otherwise go into all three places: the image is
runtime-only and runs no tests.

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

`kiewer_render_dev`'s, each a kind of `kiewer_viewer::error::Error` (`Exit`
there, the same way); 2 is a usage error.

| Code | Kind | Meaning |
|---|---|---|
| 3 | Io | reading the board or writing a PNG |
| 4 | Board | the board does not parse or has nothing to draw |
| 5 | Gpu | no Vulkan adapter, no device, or a frame that would not read back |
| 6 | Png | a PNG that would not encode or decode |

## Infrastructure

GCP and Fastly are Terraform in `infra/`, applied by hand from a workstation;
CI only deploys Cloud Run revisions with the identity it defines there.
`infra/README.md` is the procedure: bootstrapping the state bucket, the apply,
setting the three repository variables from the outputs, and what a plan after
a CI deploy should show.

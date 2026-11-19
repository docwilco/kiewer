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

## Deploy

Cloud Run in europe-west4, images in Artifact Registry under `kiewer/kiewer`. Every push to main builds the image from the runner's release build (`scripts/release-build.sh`, handed to the image job as a workflow artifact), pushes it as its short hash and deploys that digest to `kiewer-staging`. CI cuts the Release: the `release` job, last in main's run and gated on every other job, finds the manifest's Version above every Release tag in git (`scripts/release-cut.sh`), deploys that image to `kiewer`, names it `:X.Y.Z` and `:latest`, pushes the annotated tag and prunes the registry. A Release candidate (`X.Y.Z-rc.N`) is named `:X.Y.Z-rc.N`, never `:latest`, and becomes a no-traffic revision under the Cloud Run tag `rc`. A Version already tagged, or below the newest Release, is a plain push and ends at staging. The day-to-day rule (the Version bump as its own commit, the push) is the `release-conventions` skill: invoke it before a bump, a release or a deploy. A hand deploy or a rollback is `scripts/deploy-kiewer.sh <service> <Version>`, naming a kept Release. The registry keeps 3 Releases, 1 candidate and the 5 most recent images: `scripts/prune-images.sh` owns the 3 and the 1, read from git's tags, and the cleanup policy in `infra/` holds the 5 and deletes what falls out. The CI identity (repository variables `GCP_PROJECT_ID`, `GCP_WIF_PROVIDER`, `GCP_DEPLOY_SA`) comes from Terraform in `infra/`. The shipping jobs and the test job pin `runs-on: ubuntu-24.04` to match the Dockerfile's base; the Dockerfile's comment has the rule for moving either. Breaking (a `0.x` minor): a one-way database migration, a retired environment variable, a changed URL.

# Infrastructure

The GCP and Fastly side of kiewer as Terraform, applied by hand from a workstation. CI never applies; it deploys Cloud Run revisions with the identity `ci.tf` defines. One file per concern, each with the reasoning in its header comment: `registry.tf` (Artifact Registry and its cleanup policy), `storage.tf` (the blob buckets and Fastly's HMAC key), `database.tf` (Cloud SQL), `secrets.tf` (Secret Manager), `run.tf` (the two services, their accounts and Tasks queues), `ci.tf` (Workload Identity Federation and the deploy account), `fastly.tf` with `fastly/` (the service and its VCL), `outputs.tf` (what CI reads).

## Prerequisites

- `terraform` at the version in `.terraform-version` (`tfenv install` reads it; or the zip release from releases.hashicorp.com).
- `gcloud` signed in to an account that owns the project, and application-default credentials for the provider: `gcloud auth application-default login`.
- `FASTLY_API_KEY` in the environment: a Fastly API token with engineer scope on the account (the provider reads the variable).
- A GCP project with billing enabled. This configuration creates no project.
- `gh` signed in with admin on the repository, for the variables.

## Bootstrap, once

The state bucket is the one thing made outside Terraform, since Terraform needs it before it can run.

```sh
project=<project id>
gcloud storage buckets create "gs://$project-kiewer-tfstate" --project "$project" \
  --location europe-west4 --uniform-bucket-level-access --public-access-prevention
gcloud storage buckets update "gs://$project-kiewer-tfstate" --versioning
cd infra
terraform init -backend-config="bucket=$project-kiewer-tfstate"
cp terraform.tfvars.example terraform.tfvars
```

Fill `project_id` in `terraform.tfvars` (git-ignored). Done when `terraform init` has printed "successfully initialized" and `terraform validate` says the configuration is valid.

## Apply by hand

```sh
terraform plan -out=kiewer.tfplan
terraform apply kiewer.tfplan
```

Read the plan before the apply: on the first run it creates everything; after that it should name only what was changed in this directory. The first apply takes about a quarter of an hour, most of it Cloud SQL. A resource failing with an API-not-enabled error on the first run is the API enablement still propagating: run the plan and apply again. Done when the apply ends without an error and a fresh `terraform plan` reports no changes.

## After the apply

1. The repository variables CI reads, from the outputs of the same names:

   ```sh
   for v in GCP_PROJECT_ID GCP_WIF_PROVIDER GCP_DEPLOY_SA; do
     gh variable set "$v" --repo docwilco/kiewer --body "$(terraform output -raw "$v")"
   done
   ```

   Done when `gh variable list --repo docwilco/kiewer` shows the three and the next push to main's Image job runs its "Push the image as its short hash" and "Deploy to staging" steps instead of skipping them.

2. DNS: CNAME the two hostnames (`terraform output hostnames`) to the same Fastly target the other `*.drwil.co` names use; the certificate already covers them.

3. The values that exist only once the services have hostnames: register the OAuth applications per environment and provider (callback URLs on the hostnames), make a Fastly API token with purge scope on the `kiewer` service, put them in `terraform.tfvars` (`oauth_clients`, `fastly_api_token`; the example file shows the shape), and apply again. Until then the services get no variables for them and the server runs with sign-in off and no purges.

## What a plan shows after a CI deploy

Nothing. CI changes only the image and what `gcloud run deploy` writes with it, and `run.tf` ignores those fields. A plan that wants to change a service after a deploy means gcloud began writing a new field: add it to the `ignore_changes` list rather than applying the plan, which would roll the service back to the placeholder image.

## The server's environment

What `run.tf` gives each service, the names `kiewer_lib::config` reads. Secret-backed values come from Secret Manager's latest version. A provider without a registration in `oauth_clients` gets neither of its two variables and its secret has no version, which the server reads as that provider turned off; the same for the Fastly token while `fastly_api_token` is empty. Nothing is ever set to a placeholder.

| Variable | Value |
| --- | --- |
| `PUBLIC_URL` | `https://<hostname>`, for absolute URLs and the sign-in callbacks |
| `DATABASE_URL` | secret: `postgres://<role>:<password>@/<database>?host=/cloudsql/<connection name>` |
| `BLOB_BUCKET` | the environment's blob bucket, which makes the GCS store the blob store; objects at `<first two hex>/<hash>.gz` |
| `THUMBNAIL_QUEUE` | the environment's Cloud Tasks queue, as `projects/…/locations/…/queues/…` |
| `FASTLY_SERVICE_ID` | the service to purge on |
| `FASTLY_API_TOKEN` | secret: the token to purge with; only once given |
| `OAUTH_<GITHUB,DISCORD,GOOGLE>_CLIENT_ID` | the registration's client id; only once registered |
| `OAUTH_<GITHUB,DISCORD,GOOGLE>_CLIENT_SECRET` | secret: the registration's client secret; only once registered |

Cloud Run sets `PORT`. A retired variable is a breaking Release (AGENTS.md, Deploy).

## What the state holds

The database passwords, the HMAC secret Fastly signs with, the OAuth client secrets and the Fastly token are all in the state, which is why the state bucket is private and versioned. Rotation is a replace and an apply: `terraform apply -replace='random_password.database["production"]'` for a database password, `-replace=google_storage_hmac_key.fastly` for the HMAC key; a new OAuth secret or Fastly token is a tfvars edit and an apply.

## Guards against a destroy

The Cloud SQL instance and both Cloud Run services carry `deletion_protection`, and the buckets refuse to go while they hold objects. A `terraform destroy` stops at each of these until it is lifted by hand in the file, which is the intent.

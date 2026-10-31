# The two environments, each a Cloud Run service, a database on the
# shared instance, a bucket, a Tasks queue, a runtime service account and
# its secrets (docs/v1-scope.md, Q59). Every per-environment resource is a
# for_each over this map, keyed by the environment's name, so the two
# never drift apart in shape.
locals {
  environments = {
    production = {
      service  = "kiewer"
      hostname = var.production_hostname
      database = "kiewer"
    }
    staging = {
      service  = "kiewer-staging"
      hostname = var.staging_hostname
      database = "kiewer_staging"
    }
  }

  oauth_providers = toset(["github", "discord", "google"])

  # Every (environment, provider) pair, for the OAuth secrets.
  oauth_registrations = {
    for pair in setproduct(keys(local.environments), local.oauth_providers) :
    "${pair[0]}-${pair[1]}" => { environment = pair[0], provider = pair[1] }
  }

  # Bucket names are global, so the project id prefixes them.
  blob_bucket = { for env, e in local.environments : env => "${var.project_id}-${e.service}-blobs" }

  # Cloud Run's deterministic URL, known before the service exists:
  # https://<service>-<project number>.<region>.run.app. Fastly's backends
  # use it instead of the service resource's uri so the Fastly service does
  # not depend on the Cloud Run services, whose environment carries the
  # Fastly service id for purges; the other way round would be a cycle.
  run_host = { for env, e in local.environments : env => "${e.service}-${data.google_project.this.number}.${var.region}.run.app" }

  # The object key of a blob in a bucket, the same layout the filesystem
  # store uses (kiewer_lib/src/blob): <first two hex>/<hash>.gz. Fastly's
  # rewrite in fastly/recv.vcl.tftpl and the GCS store spell the same path.
  blob_key_layout = "<first two hex>/<hash>.gz"
}

data "google_project" "this" {
  project_id = var.project_id
}

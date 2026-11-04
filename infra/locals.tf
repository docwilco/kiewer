# The two environments, each a Cloud Run service, a database on the
# shared instance, a bucket, a Tasks queue, a runtime service account and
# its secrets (docs/v1-scope.md, Q59). Every per-environment resource is a
# for_each over this map, keyed by the environment's name, so the two
# never drift apart in shape.
locals {
  environments = {
    # surrogate_key_prefix goes in front of every Surrogate-Key the
    # service names and purges (kiewer_lib::cache::KeyPrefix): both
    # environments share one Fastly service, whose purge by key acts on
    # all of it, so staging's keys carry a prefix and production's none.
    production = {
      service              = "kiewer"
      hostname             = var.production_hostname
      database             = "kiewer"
      surrogate_key_prefix = ""
    }
    staging = {
      service              = "kiewer-staging"
      hostname             = var.staging_hostname
      database             = "kiewer_staging"
      surrogate_key_prefix = "staging-"
    }
  }

  oauth_providers = toset(["github", "discord", "google"])

  # Every (environment, provider) pair, for the OAuth secrets: a secret
  # exists for each, a version only for the registered ones.
  oauth_registrations = {
    for pair in setproduct(keys(local.environments), local.oauth_providers) :
    "${pair[0]}-${pair[1]}" => { environment = pair[0], provider = pair[1] }
  }

  # Per environment, the providers var.oauth_clients has a registration
  # for. Which providers exist is not a secret; the map they come from is,
  # so the set is marked nonsensitive to key resources and blocks.
  oauth_registered = {
    for env in keys(local.environments) : env => toset([
      for provider in local.oauth_providers : provider
      if nonsensitive(can(var.oauth_clients[env][provider]))
    ])
  }

  fastly_api_token_given = nonsensitive(var.fastly_api_token != "")

  # Bucket names are global, so the project id prefixes them.
  blob_bucket = { for env, e in local.environments : env => "${var.project_id}-${e.service}-blobs" }

  # Cloud Run's deterministic URL, known before the service exists:
  # https://<service>-<project number>.<region>.run.app. Fastly's backends
  # use it instead of the service resource's uri so the Fastly service does
  # not depend on the Cloud Run services, whose environment carries the
  # Fastly service id for purges; the other way round would be a cycle.
  run_host = { for env, e in local.environments : env => "${e.service}-${data.google_project.this.number}.${var.region}.run.app" }
}

# The object key of a blob in a bucket is <first two hex>/<hash>.gz, the
# layout every blob store writes (kiewer_lib/src/blob, object_name); the
# rewrite in fastly/recv.vcl.tftpl spells the same path.

# The project's number, for the deterministic Cloud Run host. Read after
# the Resource Manager API is on, which on a fresh project it may not be;
# the price is that the number, and the hosts built from it, are known
# only at the first apply.
data "google_project" "this" {
  project_id = var.project_id
  depends_on = [google_project_service.apis]
}

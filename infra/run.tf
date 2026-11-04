# One Cloud Run service per environment, running as its own service
# account with the least it needs: its database, its bucket, its secrets,
# its Tasks queue, and the right to call itself (the queue delivers
# thumbnail retries to the service as that account, ADR 0004).
#
# Cloud Scheduler calls the collector each night the same way (the job at
# the end of this file).
#
# Terraform owns the service's shape: the account, the environment, the
# Cloud SQL mount, scaling. CI owns the image: every push to main deploys
# a new revision to kiewer-staging with `gcloud run deploy --image`, the
# run that cuts a Release does the same to kiewer (.github/workflows/ci.yml),
# and a candidate becomes a no-traffic revision under the Cloud Run tag
# `rc`. The lifecycle block below ignores what those deploys change, so a
# plan after a deploy is clean; a new field gcloud starts writing shows up
# as drift there and joins the list.

resource "google_service_account" "run" {
  for_each = local.environments

  account_id   = "${each.value.service}-run"
  display_name = "Runtime of the ${each.value.service} Cloud Run service"
  depends_on   = [google_project_service.apis]
}

resource "google_project_iam_member" "run_connects_to_sql" {
  for_each = local.environments

  project = var.project_id
  role    = "roles/cloudsql.client"
  member  = google_service_account.run[each.key].member
}

# objectUser: create, read, list and delete objects, no ACL and no bucket
# administration. The collector's deletes and the upload's writes are all
# the server does to its bucket.
resource "google_storage_bucket_iam_member" "run_uses_blobs" {
  for_each = local.environments

  bucket = google_storage_bucket.blobs[each.key].name
  role   = "roles/storage.objectUser"
  member = google_service_account.run[each.key].member
}

resource "google_cloud_tasks_queue" "thumbnails" {
  for_each = local.environments

  name     = "${each.value.service}-thumbnails"
  location = var.region

  rate_limits {
    max_concurrent_dispatches = 2
    max_dispatches_per_second = 5
  }

  retry_config {
    max_attempts  = 5
    min_backoff   = "10s"
    max_backoff   = "300s"
    max_doublings = 4
  }

  depends_on = [google_project_service.apis]
}

resource "google_cloud_tasks_queue_iam_member" "run_enqueues_thumbnails" {
  for_each = local.environments

  name     = google_cloud_tasks_queue.thumbnails[each.key].name
  location = var.region
  role     = "roles/cloudtasks.enqueuer"
  member   = google_service_account.run[each.key].member
}

# A task carries an OIDC token minted as the runtime account, which takes
# actAs on that account; the account grants it to itself.
resource "google_service_account_iam_member" "run_acts_as_itself" {
  for_each = local.environments

  service_account_id = google_service_account.run[each.key].name
  role               = "roles/iam.serviceAccountUser"
  member             = google_service_account.run[each.key].member
}

locals {
  # The environment per service, the names kiewer_lib::config reads;
  # README.md lists them. A provider with no registration in
  # var.oauth_clients gets no variables at all, neither id nor secret,
  # which the server reads as that provider turned off; a placeholder
  # value would read as a registration. The same for the Fastly token.
  # The registered set is derived from a sensitive variable, so it is
  # marked nonsensitive to key the env blocks: which providers exist is
  # not a secret, their values are.
  run_env = {
    for env, e in local.environments : env => merge(
      {
        PUBLIC_URL      = "https://${e.hostname}"
        BLOB_BUCKET     = google_storage_bucket.blobs[env].name
        THUMBNAIL_QUEUE = google_cloud_tasks_queue.thumbnails[env].id
        # The queue's tasks carry an OIDC token minted as this account,
        # and the server checks the token names it.
        THUMBNAIL_TASK_ACCOUNT = google_service_account.run[env].email
        FASTLY_SERVICE_ID      = fastly_service_vcl.kiewer.id
      },
      {
        for provider in local.oauth_registered[env] :
        "OAUTH_${upper(provider)}_CLIENT_ID" => var.oauth_clients[env][provider].client_id
      },
    )
  }
  run_secret_env = {
    for env, e in local.environments : env => merge(
      { DATABASE_URL = google_secret_manager_secret.database_url[env].secret_id },
      local.fastly_api_token_given ? { FASTLY_API_TOKEN = google_secret_manager_secret.fastly_api_token.secret_id } : {},
      {
        for provider in local.oauth_registered[env] :
        "OAUTH_${upper(provider)}_CLIENT_SECRET" => google_secret_manager_secret.oauth_client_secret["${env}-${provider}"].secret_id
      },
    )
  }
}

resource "google_cloud_run_v2_service" "app" {
  for_each = local.environments

  name     = each.value.service
  location = var.region
  # Fastly reaches the service over the public internet (ADR 0003).
  ingress             = "INGRESS_TRAFFIC_ALL"
  deletion_protection = true

  template {
    service_account = google_service_account.run[each.key].email

    scaling {
      min_instance_count = 0
      max_instance_count = 3
    }

    # An upload parses for up to 30 s (docs/v1-scope.md, Q13); the rest is
    # headroom for a slow multipart body.
    timeout = "300s"

    containers {
      # A placeholder for the first revision only: CI deploys the real
      # image, and the lifecycle block keeps Terraform from putting this
      # back.
      image = "us-docker.pkg.dev/cloudrun/container/hello"

      resources {
        limits = {
          cpu    = "1"
          memory = "512Mi"
        }
        cpu_idle          = true
        startup_cpu_boost = true
      }

      dynamic "env" {
        for_each = local.run_env[each.key]
        content {
          name  = env.key
          value = env.value
        }
      }

      dynamic "env" {
        for_each = local.run_secret_env[each.key]
        content {
          name = env.key
          value_source {
            secret_key_ref {
              secret  = env.value
              version = "latest"
            }
          }
        }
      }

      volume_mounts {
        name       = "cloudsql"
        mount_path = "/cloudsql"
      }
    }

    volumes {
      name = "cloudsql"
      cloud_sql_instance {
        instances = [google_sql_database_instance.kiewer.connection_name]
      }
    }
  }

  lifecycle {
    ignore_changes = [
      # What a CI deploy writes: the image, the client stamp, the revision
      # name and annotations gcloud adds, and the traffic split (a
      # candidate's revision sits at zero traffic under the tag `rc`).
      template[0].containers[0].image,
      template[0].revision,
      template[0].labels,
      template[0].annotations,
      client,
      client_version,
      traffic,
    ]
  }

  depends_on = [
    google_project_service.apis,
    google_secret_manager_secret_version.database_url,
    google_secret_manager_secret_version.oauth_client_secret,
    google_secret_manager_secret_version.fastly_api_token,
    google_secret_manager_secret_iam_member.run_reads_database_url,
    google_secret_manager_secret_iam_member.run_reads_oauth_client_secret,
    google_secret_manager_secret_iam_member.run_reads_fastly_api_token,
  ]
}

# Anyone may call the service: Fastly is the front, and the server also
# answers directly on its run.app host.
resource "google_cloud_run_v2_service_iam_member" "public_invoker" {
  for_each = local.environments

  name     = google_cloud_run_v2_service.app[each.key].name
  location = var.region
  role     = "roles/run.invoker"
  member   = "allUsers"
}

# The queue's deliveries, as the runtime account.
resource "google_cloud_run_v2_service_iam_member" "run_invokes_itself" {
  for_each = local.environments

  name     = google_cloud_run_v2_service.app[each.key].name
  location = var.region
  role     = "roles/run.invoker"
  member   = google_service_account.run[each.key].member
}

# The collector (ADR 0002): one authenticated POST a night. The token is
# minted as the runtime account for the public URL, which is what the
# server's check takes (kiewer_lib::thumbnail::TaskAuth), so this route
# and the thumbnail retry share one caller. The URL is the run.app host and
# not the public one: the handler answers when the run ends, and Fastly's
# first byte timeout (fastly.tf) is well short of the service's. The run is
# idempotent, and one cut short by the deadline, the service's own timeout,
# leaves its work for the retry or the next night.
resource "google_cloud_scheduler_job" "collector" {
  for_each = local.environments

  name             = "${each.value.service}-collector"
  region           = var.region
  schedule         = "0 3 * * *"
  time_zone        = "Etc/UTC"
  attempt_deadline = "300s"

  retry_config {
    retry_count = 1
  }

  http_target {
    http_method = "POST"
    uri         = "https://${local.run_host[each.key]}/api/collector"

    oidc_token {
      service_account_email = google_service_account.run[each.key].email
      audience              = "https://${each.value.hostname}"
    }
  }

  depends_on = [google_project_service.apis]
}

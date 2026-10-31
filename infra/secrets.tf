# Secret Manager holds what the server must not get from the image
# (Dockerfile) or from plain environment: the database URL per
# environment, the OAuth client secrets per environment and provider
# (docs/v1-scope.md, Q57), and the Fastly token the server purges with.
# run.tf mounts each as an environment variable from the secret's latest
# version. Every value here is also in the Terraform state, which is why
# the state bucket is private and versioned (README.md, Bootstrap).

resource "google_secret_manager_secret" "database_url" {
  for_each = local.environments

  secret_id = "${each.value.service}-database-url"
  replication {
    auto {}
  }
  depends_on = [google_project_service.apis]
}

# postgres://<role>:<password>@/<db>?host=/cloudsql/<connection name>: the
# empty host and the host= query parameter are how sqlx names a unix
# socket directory, and /cloudsql/<connection name> is where Cloud Run
# puts the instance's socket (run.tf, the cloudsql volume).
resource "google_secret_manager_secret_version" "database_url" {
  for_each = local.environments

  secret      = google_secret_manager_secret.database_url[each.key].id
  secret_data = "postgres://${google_sql_user.app[each.key].name}:${random_password.database[each.key].result}@/${google_sql_database.app[each.key].name}?host=/cloudsql/${google_sql_database_instance.kiewer.connection_name}"
}

resource "google_secret_manager_secret" "oauth_client_secret" {
  for_each = local.oauth_registrations

  secret_id = "${local.environments[each.value.environment].service}-oauth-${each.value.provider}-client-secret"
  replication {
    auto {}
  }
  depends_on = [google_project_service.apis]
}

# A version for the registered providers only (run.tf gives the service
# the variables for those alone); an unregistered provider's secret
# waits empty for its first version.
resource "google_secret_manager_secret_version" "oauth_client_secret" {
  for_each = {
    for key, r in local.oauth_registrations : key => r
    if contains(local.oauth_registered[r.environment], r.provider)
  }

  secret      = google_secret_manager_secret.oauth_client_secret[each.key].id
  secret_data = var.oauth_clients[each.value.environment][each.value.provider].client_secret
}

# One token for both environments: the two hostnames are one Fastly
# service (fastly.tf), so a purge from either is a purge on it.
resource "google_secret_manager_secret" "fastly_api_token" {
  secret_id = "kiewer-fastly-api-token"
  replication {
    auto {}
  }
  depends_on = [google_project_service.apis]
}

resource "google_secret_manager_secret_version" "fastly_api_token" {
  count = local.fastly_api_token_given ? 1 : 0

  secret      = google_secret_manager_secret.fastly_api_token.id
  secret_data = var.fastly_api_token
}

# Each runtime service account reads its own environment's secrets and
# the shared Fastly token, nothing else.
resource "google_secret_manager_secret_iam_member" "run_reads_database_url" {
  for_each = local.environments

  secret_id = google_secret_manager_secret.database_url[each.key].secret_id
  role      = "roles/secretmanager.secretAccessor"
  member    = google_service_account.run[each.key].member
}

resource "google_secret_manager_secret_iam_member" "run_reads_oauth_client_secret" {
  for_each = local.oauth_registrations

  secret_id = google_secret_manager_secret.oauth_client_secret[each.key].secret_id
  role      = "roles/secretmanager.secretAccessor"
  member    = google_service_account.run[each.value.environment].member
}

resource "google_secret_manager_secret_iam_member" "run_reads_fastly_api_token" {
  for_each = local.environments

  secret_id = google_secret_manager_secret.fastly_api_token.secret_id
  role      = "roles/secretmanager.secretAccessor"
  member    = google_service_account.run[each.key].member
}

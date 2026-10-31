# The three repository variables CI reads (.github/workflows/ci.yml),
# named as CI names them so `gh variable set` reads straight from
# `terraform output -raw` (README.md, After the apply).
output "GCP_PROJECT_ID" {
  description = "Repository variable: the project the registry and the services are in."
  value       = var.project_id
}

output "GCP_WIF_PROVIDER" {
  description = "Repository variable: the Workload Identity Federation provider's full resource name, for google-github-actions/auth."
  value       = google_iam_workload_identity_pool_provider.github_actions.name
}

output "GCP_DEPLOY_SA" {
  description = "Repository variable: the service account CI impersonates."
  value       = google_service_account.deploy.email
}

output "cloud_run_hosts" {
  description = "Per environment, the run.app host Fastly sends to; also reachable directly."
  value       = local.run_host
}

output "cloud_sql_connection_name" {
  description = "The instance's connection name, the directory under /cloudsql the socket is in."
  value       = google_sql_database_instance.kiewer.connection_name
}

output "blob_buckets" {
  description = "Per environment, the blob bucket."
  value       = local.blob_bucket
}

output "fastly_service_id" {
  description = "The Fastly service in front of both hostnames; the server purges by Surrogate-Key on it."
  value       = fastly_service_vcl.kiewer.id
}

output "hostnames" {
  description = "The two hostnames to CNAME to Fastly, as the existing *.drwil.co setup does for its other names."
  value       = { for env, e in local.environments : env => e.hostname }
}

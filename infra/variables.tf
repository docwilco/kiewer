variable "project_id" {
  description = "The GCP project everything lives in. It exists already, with billing enabled; this configuration creates no project."
  type        = string
}

variable "region" {
  description = "The one region for Cloud Run, Cloud SQL, the buckets, the registry and the Tasks queues (docs/v1-scope.md, Q16)."
  type        = string
  default     = "europe-west4"
}

variable "github_repository" {
  description = "The GitHub repository whose Actions may impersonate the deploy service account, as owner/name."
  type        = string
  default     = "docwilco/kiewer"
}

variable "production_hostname" {
  description = "The hostname Fastly routes to the production service. DNS and the TLS certificate are managed outside this configuration (the existing *.drwil.co certificate at Fastly)."
  type        = string
  default     = "kiewer.drwil.co"
}

variable "staging_hostname" {
  description = "The hostname Fastly routes to the staging service."
  type        = string
  default     = "kiewer-staging.drwil.co"
}

variable "database_tier" {
  description = "The Cloud SQL machine tier. A shared-core tier is enough for v1 and is the cheapest Postgres Cloud SQL offers."
  type        = string
  default     = "db-f1-micro"
}

variable "oauth_clients" {
  description = <<-EOT
    The OAuth registrations per environment (production, staging) and
    provider (github, discord, google): client id and client secret. Each
    environment has its own registrations (docs/v1-scope.md, Q57). An
    environment or provider left out gets no variables and no secret
    version, which the server reads as that provider turned off, so the
    first apply needs no registration done yet.
  EOT
  type = map(map(object({
    client_id     = string
    client_secret = string
  })))
  default   = {}
  sensitive = true
}

variable "fastly_api_token" {
  description = "A Fastly API token with purge scope on the kiewer service, for the server's Surrogate-Key purges. Made by hand in the Fastly control panel (the provider cannot mint tokens). Empty means the services get no FASTLY_API_TOKEN and the secret has no version until a token is applied."
  type        = string
  default     = ""
  sensitive   = true
}

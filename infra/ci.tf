# The identity CI deploys with, keyless: GitHub Actions presents its OIDC
# token to a Workload Identity Federation pool, which lets a run from
# docwilco/kiewer on main impersonate the deploy service account. The
# account can push to the registry, delete tags there and deploy the two
# services, nothing else (.github/workflows/ci.yml, the image and release
# jobs). The three outputs in outputs.tf are what CI reads as repository
# variables.

resource "google_iam_workload_identity_pool" "github" {
  workload_identity_pool_id = "github"
  display_name              = "GitHub Actions"
  depends_on                = [google_project_service.apis]
}

resource "google_iam_workload_identity_pool_provider" "github_actions" {
  workload_identity_pool_id          = google_iam_workload_identity_pool.github.workload_identity_pool_id
  workload_identity_pool_provider_id = "github-actions"
  display_name                       = "GitHub Actions OIDC"

  attribute_mapping = {
    "google.subject"       = "assertion.sub"
    "attribute.repository" = "assertion.repository"
    "attribute.ref"        = "assertion.ref"
  }

  # Only this repository, and only the ref whose runs ship: a push to
  # main (the image and release jobs). A pull request's run never
  # authenticates (its steps are gated off), and a token from any other
  # ref is refused here.
  attribute_condition = "assertion.repository == \"${var.github_repository}\" && assertion.ref == \"refs/heads/main\""

  oidc {
    issuer_uri = "https://token.actions.githubusercontent.com"
  }
}

resource "google_service_account" "deploy" {
  account_id   = "kiewer-deploy"
  display_name = "CI: push images and deploy Cloud Run revisions"
  depends_on   = [google_project_service.apis]
}

resource "google_service_account_iam_member" "deploy_impersonated_by_github" {
  service_account_id = google_service_account.deploy.name
  role               = "roles/iam.workloadIdentityUser"
  member             = "principalSet://iam.googleapis.com/${google_iam_workload_identity_pool.github.name}/attribute.repository/${var.github_repository}"
}

# Push and tag images (the release job adds :<Version> and :latest to a
# pushed manifest) and delete tags (scripts/prune-images.sh). repoAdmin is
# writer plus the deletes; read, which imagetools inspect needs, comes
# with it.
resource "google_artifact_registry_repository_iam_member" "deploy_manages_images" {
  location   = google_artifact_registry_repository.kiewer.location
  repository = google_artifact_registry_repository.kiewer.name
  role       = "roles/artifactregistry.repoAdmin"
  member     = google_service_account.deploy.member
}

# Deploy revisions to the two services, and nothing project-wide.
resource "google_cloud_run_v2_service_iam_member" "deploy_develops_service" {
  for_each = local.environments

  name     = google_cloud_run_v2_service.app[each.key].name
  location = var.region
  role     = "roles/run.developer"
  member   = google_service_account.deploy.member
}

# A deploy of a service that runs as an account takes actAs on it.
resource "google_service_account_iam_member" "deploy_acts_as_run" {
  for_each = local.environments

  service_account_id = google_service_account.run[each.key].name
  role               = "roles/iam.serviceAccountUser"
  member             = google_service_account.deploy.member
}

# One private bucket per environment for blobs (ADR 0002): objects under
# <first two hex>/<hash>.gz, gzip bytes with Content-Encoding: gzip set by
# the store. Nothing public: readers reach a blob through Fastly, which
# signs its origin requests with the HMAC key below (ADR 0003), and the
# server reaches its bucket as its runtime service account (run.tf).
resource "google_storage_bucket" "blobs" {
  for_each = local.environments

  name          = local.blob_bucket[each.key]
  location      = var.region
  storage_class = "STANDARD"

  uniform_bucket_level_access = true
  public_access_prevention    = "enforced"
  # The collector (ADR 0002) deletes blobs; a destroy of this configuration
  # never takes a bucket with objects in it.
  force_destroy = false

  depends_on = [google_project_service.apis]
}

# The identity Fastly reads blobs as: a service account with read on both
# buckets, and one HMAC key for it. The key's secret lands in the Fastly
# VCL (fastly.tf) and in the Terraform state; rotating it is a taint of
# this resource and an apply.
resource "google_service_account" "fastly" {
  account_id   = "kiewer-fastly"
  display_name = "Fastly origin reads of the kiewer blob buckets"
  depends_on   = [google_project_service.apis]
}

resource "google_storage_bucket_iam_member" "fastly_reads_blobs" {
  for_each = local.environments

  bucket = google_storage_bucket.blobs[each.key].name
  role   = "roles/storage.objectViewer"
  member = google_service_account.fastly.member
}

resource "google_storage_hmac_key" "fastly" {
  service_account_email = google_service_account.fastly.email
}

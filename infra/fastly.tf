# One Fastly service in front of both hostnames (ADR 0003). The hostname
# picks the environment; within it, GET and HEAD of /blob/<hash> go to the
# environment's bucket with an HMAC-signed origin request, everything else
# to the environment's Cloud Run service. The VCL lives in fastly/*.vcl
# and fastly/*.vcl.tftpl, one file per subroutine; the comments there
# explain each step.
#
# TLS and DNS are outside this configuration: both hostnames sit under the
# existing *.drwil.co certificate at Fastly (docs/v1-scope.md, Q1), and
# the CNAMEs are made by hand (README.md, After the apply).
#
# Surrogate-Key purges need no configuration: Fastly indexes the
# Surrogate-Key header every origin response carries and strips it before
# the client. The server purges with the token in secrets.tf. A purge by
# key acts on the whole service, so staging's keys carry a prefix
# (surrogate_key_prefix in locals.tf) and its purges leave production's
# pages alone.

resource "fastly_service_vcl" "kiewer" {
  name     = "kiewer"
  activate = true

  # A response with no caching headers is not cached. The server names a
  # policy on every response (cacheable pages, private no-store pages,
  # never-cached /api/me), and the fetch snippet names one for blobs.
  default_ttl = 0

  stale_if_error = true

  dynamic "domain" {
    for_each = local.environments
    content {
      name    = domain.value.hostname
      comment = "kiewer ${domain.key}"
    }
  }

  # The Cloud Run origins. Fastly connects to the run.app host over TLS
  # and sends it as the Host header (Cloud Run routes on it); the client's
  # hostname travels in X-Forwarded-Host, set in recv. No load balancing
  # across backends: recv chooses one by name.
  dynamic "backend" {
    for_each = local.environments
    content {
      name              = "run_${backend.key}"
      address           = local.run_host[backend.key]
      port              = 443
      use_ssl           = true
      ssl_check_cert    = true
      ssl_cert_hostname = local.run_host[backend.key]
      ssl_sni_hostname  = local.run_host[backend.key]
      override_host     = local.run_host[backend.key]
      auto_loadbalance  = false
      # An upload parses for up to 30 s before its response starts.
      first_byte_timeout    = 60000
      between_bytes_timeout = 30000
      connect_timeout       = 5000
    }
  }

  # The one GCS origin for both buckets: path-style requests, with the
  # bucket as the first path segment (recv rewrites the URL).
  backend {
    name              = "gcs"
    address           = "storage.googleapis.com"
    port              = 443
    use_ssl           = true
    ssl_check_cert    = true
    ssl_cert_hostname = "storage.googleapis.com"
    ssl_sni_hostname  = "storage.googleapis.com"
    override_host     = "storage.googleapis.com"
    auto_loadbalance  = false
  }

  snippet {
    name     = "kiewer recv: route by hostname and path"
    type     = "recv"
    priority = 100
    content = templatefile("${path.module}/fastly/recv.vcl.tftpl", {
      staging_hostname  = var.staging_hostname
      production_bucket = local.blob_bucket["production"]
      staging_bucket    = local.blob_bucket["staging"]
    })
  }

  # The same signing in miss and pass: both are where a request becomes an
  # origin request, and a blob fetch takes either path.
  snippet {
    name     = "kiewer miss: sign the GCS request"
    type     = "miss"
    priority = 100
    content = templatefile("${path.module}/fastly/sign.vcl.tftpl", {
      access_id = google_storage_hmac_key.fastly.access_id
      secret    = google_storage_hmac_key.fastly.secret
    })
  }

  snippet {
    name     = "kiewer pass: sign the GCS request"
    type     = "pass"
    priority = 100
    content = templatefile("${path.module}/fastly/sign.vcl.tftpl", {
      access_id = google_storage_hmac_key.fastly.access_id
      secret    = google_storage_hmac_key.fastly.secret
    })
  }

  snippet {
    name     = "kiewer fetch: cache a blob forever"
    type     = "fetch"
    priority = 100
    content  = file("${path.module}/fastly/fetch.vcl")
  }

  snippet {
    name     = "kiewer deliver: strip the GCS headers"
    type     = "deliver"
    priority = 100
    content  = file("${path.module}/fastly/deliver.vcl")
  }
}

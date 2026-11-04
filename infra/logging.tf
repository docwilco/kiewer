# The renderer beacon's log line (docs/v1-scope.md, Telemetry; the line
# is written in kiewer_lib/src/web/beacon.rs), counted per environment
# with the line's closed-set fields as labels, so Metrics Explorer splits
# page loads by backend, outcome, WebGPU presence and browser family. The
# server writes the line as one JSON object on stdout (LOG_FORMAT,
# kiewer/src/log.rs), which Cloud Run's log agent reads into
# jsonPayload. The JSON layer camelCases each field's key as it writes
# the line, so an extractor names the written key (webgpuPresent for the
# code's webgpu_present) while the label keeps the code's name;
# kiewer_lib/tests/it/beacon.rs reads this file and checks the filter and
# the extractors against a real line. The vendor is in the line but not a
# label: it is a string the client chose, and a label of unbounded
# cardinality is a metric nobody can chart.
resource "google_logging_metric" "renderer_beacon" {
  for_each = local.environments

  name        = "${each.value.service}-renderer-beacon"
  description = "Renderer beacons from the viewer page of ${each.value.service}, one per page load"
  filter      = <<-EOT
    resource.type="cloud_run_revision"
    resource.labels.service_name="${each.value.service}"
    jsonPayload.target="kiewer_lib::web::beacon"
    jsonPayload.message="renderer beacon"
  EOT

  metric_descriptor {
    metric_kind = "DELTA"
    value_type  = "INT64"

    labels {
      key         = "backend"
      value_type  = "STRING"
      description = "webgpu, webgl2, or none when neither came up"
    }
    labels {
      key         = "outcome"
      value_type  = "STRING"
      description = "ok, failed, panic, or capped"
    }
    labels {
      key         = "webgpu_present"
      value_type  = "BOOL"
      description = "whether navigator.gpu existed, whatever came of it"
    }
    labels {
      key         = "user_agent_family"
      value_type  = "STRING"
      description = "chrome, firefox, safari, edge or other"
    }
  }

  label_extractors = {
    backend           = "EXTRACT(jsonPayload.backend)"
    outcome           = "EXTRACT(jsonPayload.outcome)"
    webgpu_present    = "EXTRACT(jsonPayload.webgpuPresent)"
    user_agent_family = "EXTRACT(jsonPayload.userAgentFamily)"
  }

  depends_on = [google_project_service.apis]
}

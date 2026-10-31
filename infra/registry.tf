# The image registry CI pushes to: <region>-docker.pkg.dev/<project>/kiewer/kiewer
# (AGENTS.md, Deploy). Every push to main pushes the image as :<short
# hash>; a Release tag's run adds :<Version> and, for a plain Release,
# :latest; a candidate's is :<Version>-rc.N. The cleanup policy below is
# what AGENTS.md describes as "3 Releases, 1 candidate, 5 unreleased
# images kept", spelled in what Artifact Registry can express.
#
# Artifact Registry counts versions per package, never per tag: a
# most_recent_versions keep takes a package prefix and a count, and a
# condition takes tag prefixes but no count. So the policy is:
#
#   keep-releases    every image tagged like a Release or a candidate
#                    (X.Y.Z, X.Y.Z-rc.N, latest). This is the "3 Releases"
#                    and the "1 candidate", as a floor: those are kept,
#                    and so are the older ones, since no count applies to
#                    a tag. A Release is rare and small, and a kept one is
#                    a rollback target (scripts/deploy-kiewer.sh).
#   keep-recent      the 5 most recent versions of the package, whatever
#                    their tags. This is the "5 unreleased": the last five
#                    pushes to main, which is what a Release tag's run
#                    names (ci.yml, release job). A Release among them
#                    counts as one of the five.
#   delete-the-rest  everything else, once a day old. A keep policy beats
#                    a delete policy, so this removes only what neither
#                    keep covers: hash-tagged images beyond the last five.
#
# The day's floor on the delete keeps the image a Release tag's run is
# about to name out of reach while the tag's gate waits on main's run.
resource "google_artifact_registry_repository" "kiewer" {
  location      = var.region
  repository_id = "kiewer"
  format        = "DOCKER"
  description   = "kiewer images: :<short hash> on every push to main, :<Version> and :latest from a Release tag's run"

  docker_config {
    # The release job moves :latest and adds tags to a pushed manifest.
    immutable_tags = false
  }

  cleanup_policy_dry_run = false

  cleanup_policies {
    id     = "keep-releases"
    action = "KEEP"
    condition {
      tag_state = "TAGGED"
      # A Release tag is X.Y.Z or X.Y.Z-rc.N (scripts/release-tag.sh), so
      # it starts with the major and a dot; a short hash never contains a
      # dot. The list covers majors 0 to 9; a major of 10 adds "10.".
      tag_prefixes = ["0.", "1.", "2.", "3.", "4.", "5.", "6.", "7.", "8.", "9.", "latest"]
    }
  }

  cleanup_policies {
    id     = "keep-recent"
    action = "KEEP"
    most_recent_versions {
      keep_count = 5
    }
  }

  cleanup_policies {
    id     = "delete-the-rest"
    action = "DELETE"
    condition {
      tag_state  = "ANY"
      older_than = "1d"
    }
  }

  depends_on = [google_project_service.apis]
}

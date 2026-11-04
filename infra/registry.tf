# The image registry CI pushes to: <region>-docker.pkg.dev/<project>/kiewer/kiewer
# (AGENTS.md, Deploy). Every push to main pushes the image as :<short
# hash>; the run that cuts a Release adds :<Version> and, for a plain
# Release, :latest; a candidate's is :<Version>-rc.N (ci.yml, release job).
#
# AGENTS.md's "3 Releases, 1 candidate, the 5 most recent images" takes two
# things to hold, since Artifact Registry counts versions per package, never
# per tag: a most_recent_versions keep takes a package prefix and a count,
# and a condition takes tag prefixes but no count. The 3 and the 1 are
# scripts/prune-images.sh's: after a cut it deletes the tag of every other
# Release and candidate, leaving each such image with its :<short hash>.
# The cleanup policy does the rest:
#
#   keep-releases    every image still tagged like a Release or a candidate
#                    (X.Y.Z, X.Y.Z-rc.N, latest). No bound of its own; the
#                    prune is the bound. It has to stay: without it
#                    delete-the-rest takes a kept Release, a rollback
#                    target (scripts/deploy-kiewer.sh), once five later
#                    pushes have passed it.
#   keep-recent      the 5 most recent versions of the package, whatever
#                    their tags. A Release among them counts as one of the
#                    five.
#   delete-the-rest  everything else, once a day old. A keep policy beats
#                    a delete policy, so this removes only what neither
#                    keep covers: hash-tagged images beyond the last five,
#                    which is where an image lands when the prune takes
#                    its Release tag.
#
# The day's floor on the delete keeps an image out of reach for its first
# day, so the run that pushed it can still name it however many pushes
# land meanwhile.
resource "google_artifact_registry_repository" "kiewer" {
  location      = var.region
  repository_id = "kiewer"
  format        = "DOCKER"
  description   = "kiewer images: :<short hash> on every push to main, :<Version> and :latest from the run that cuts a Release"

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
      # A Release tag is X.Y.Z or X.Y.Z-rc.N (ci.yml, release job), so it
      # starts with the major and a dot; a short hash never contains a
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
      tag_state = "ANY"
      # A day, in the seconds form the API takes.
      older_than = "86400s"
    }
  }

  depends_on = [google_project_service.apis]
}

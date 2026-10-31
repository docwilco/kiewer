# The GCP and Fastly side of kiewer, applied by hand from a workstation
# (docs/v1-scope.md, Operations). CI never applies: it only deploys Cloud
# Run revisions with the identity defined in ci.tf. README.md in this
# directory has the procedure.
terraform {
  # .terraform-version pins the exact release a workstation runs (tfenv
  # reads it); this is the floor the configuration is written against.
  required_version = "~> 1.17"

  # State in a GCS bucket; the bucket is created once by hand before the
  # first init (README.md, Bootstrap) and named at init time:
  #   terraform init -backend-config="bucket=<project>-kiewer-tfstate"
  # A backend block takes no variables, which is why the name is passed
  # at init rather than read from var.project_id.
  backend "gcs" {
    prefix = "kiewer"
  }

  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 8.9"
    }
    fastly = {
      source  = "fastly/fastly"
      version = "~> 10.1"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.9"
    }
  }
}

provider "google" {
  project = var.project_id
  region  = var.region
}

# The API key comes from FASTLY_API_KEY in the environment, never from a
# variable, so it is in no tfvars file.
provider "fastly" {}

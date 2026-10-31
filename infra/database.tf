# One Cloud SQL Postgres instance with a database per environment
# (docs/v1-scope.md, Q59). The server connects over the Cloud Run unix
# socket /cloudsql/<connection name> (S4), which run.tf mounts; the
# instance has a public address because that is what the Cloud Run
# connection rides on, with no authorized network, so nothing reaches it
# without IAM.
resource "google_sql_database_instance" "kiewer" {
  name = "kiewer"
  # The major compose.yaml and CI's service container run; the three move
  # together.
  database_version = "POSTGRES_16"
  region           = var.region
  # A destroy of this configuration never takes the instance; lift this
  # by hand first if that is ever meant.
  deletion_protection = true

  settings {
    tier              = var.database_tier
    edition           = "ENTERPRISE"
    availability_type = "ZONAL"
    disk_type         = "PD_SSD"
    disk_size         = 10
    disk_autoresize   = true

    backup_configuration {
      enabled                        = true
      point_in_time_recovery_enabled = true
      start_time                     = "03:00"
      transaction_log_retention_days = 7
      backup_retention_settings {
        retained_backups = 14
      }
    }

    ip_configuration {
      ipv4_enabled = true
    }

    maintenance_window {
      day  = 7
      hour = 4
    }
  }

  depends_on = [google_project_service.apis]
}

resource "google_sql_database" "app" {
  for_each = local.environments

  name     = each.value.database
  instance = google_sql_database_instance.kiewer.name
}

# One role per environment, named as its database, with a generated
# password that reaches the server only through Secret Manager
# (secrets.tf, DATABASE_URL). No special characters, so the password needs
# no escaping in the URL.
resource "random_password" "database" {
  for_each = local.environments

  length  = 32
  special = false
}

resource "google_sql_user" "app" {
  for_each = local.environments

  name     = each.value.database
  instance = google_sql_database_instance.kiewer.name
  password = random_password.database[each.key].result
}

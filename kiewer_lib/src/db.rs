//! Postgres: the pool and the migration set.

use sqlx::PgPool;
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;

use crate::error::Result;

/// Every migration under `migrations/` at the repo root, embedded at
/// compile time. A file is `<version>_<description>.sql`; sqlx records each
/// one it has run in `_sqlx_migrations` and refuses a file whose content
/// changed after it ran, so a migration is never edited, only followed.
pub static MIGRATOR: Migrator = sqlx::migrate!("../migrations");

/// A pool connected to `url` with the migration set applied: what the
/// server starts with.
///
/// # Errors
///
/// When Postgres cannot be reached or a migration fails.
pub async fn connect(url: &str) -> Result<PgPool> {
    let pool = pool(url).await?;
    migrate(&pool).await?;
    Ok(pool)
}

/// A pool connected to `url`, its migrations not run: the first half of
/// [`connect`], for a server that says which half it is in.
///
/// # Errors
///
/// When Postgres cannot be reached.
pub async fn pool(url: &str) -> Result<PgPool> {
    Ok(PgPoolOptions::new().connect(url).await?)
}

/// Apply every migration not yet recorded in `_sqlx_migrations`. A second
/// run applies nothing.
///
/// # Errors
///
/// When a migration fails, or one that ran has since been edited.
pub async fn migrate(pool: &PgPool) -> Result<()> {
    MIGRATOR.run(pool).await?;
    Ok(())
}

/// A pool that connects on first use, not now: for a router under test
/// that never touches the database, or one whose health route should
/// report the database rather than refuse to start.
///
/// # Errors
///
/// When `url` is not a Postgres connection string.
pub fn lazy_pool(url: &str) -> Result<PgPool> {
    Ok(PgPoolOptions::new().connect_lazy(url)?)
}

# Migrations

The schema, as sqlx applies it: `kiewer_lib::db::MIGRATOR` embeds this
directory at compile time (`sqlx::migrate!`) and the server runs every
migration not yet recorded in `_sqlx_migrations` at startup. Empty until the
first table.

A migration is one file, `<version>_<description>.sql`, with the version a
number that sorts after every existing one (`sqlx migrate add <description>`
from sqlx-cli writes a timestamped name). sqlx checks the content of a
migration it has run and refuses a changed one, so a migration is never
edited after it reached a shared database; the fix is a later migration.
A one-way migration is a breaking change (a `0.x` minor; AGENTS.md, Deploy).

Files whose names do not match the pattern, such as this one, are ignored.

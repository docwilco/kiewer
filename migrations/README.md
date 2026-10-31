# Migrations

The schema, one file per step, `<version>_<description>.sql`, with the
version a number that sorts after every existing one; `sqlx migrate add
<description>` from sqlx-cli writes a timestamped name. Empty until the
first table. How the set is embedded and run is `kiewer_lib::db`.

A migration that has reached a shared database is never edited (sqlx
refuses a changed one); the fix is a later migration. A one-way migration
is a breaking change (a `0.x` minor; AGENTS.md, Deploy). Files whose names
do not match the pattern, such as this one, are ignored.

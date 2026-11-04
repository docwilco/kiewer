//! Statement text that several statements share, spliced in at compile
//! time so every statement stays one `sqlx` query macro checked against
//! the schema (the offline data in `.sqlx/`, README Development).
//!
//! A query macro reads its SQL before any macro inside it expands, so a
//! shared piece cannot be a `const` or a `format!` argument: the macro
//! sees only string literals, joined by `+`. `statement!` writes that
//! join. Its statement is a bracketed list of literals and fragment
//! names, and each name becomes its text:
//!
//! ```ignore
//! statement!(query_as!(DesignRow, ["SELECT " DESIGN_COLUMNS " FROM design d WHERE d.id = $1"], id as DesignId))
//! ```
//!
//! The fragments, each named once here:
//!
//! - `CHAIN`: the chain from a branch's head down through the parents,
//!   as a recursive common table expression named `chain`: one row per
//!   revision with how far behind the head it is (`name`, `hash`,
//!   `behind`). Over every branch of the design (`$1`) with `$2` null, or
//!   the one named `$2`. Each step is a primary-key probe of `revision`,
//!   so the walk costs the chains' length, never a scan; a chain cannot
//!   loop, since a revision's hash covers its parent's. Follows `WITH
//!   RECURSIVE`.
//! - `DESIGN_COLUMNS`: the columns every read of a design selects, under
//!   `design::DesignRow`'s names and types, from a `design` aliased `d`.
//!   Each is marked not null, as the table has it, so the columns keep
//!   their types where a `UNION` would hide the table from `sqlx`.
//! - `REVISION_COLUMNS`: the columns every read of a revision selects,
//!   under `design::RevisionRow`'s names and types, from a `revision`
//!   aliased `r`; `created_at` is formatted by Postgres, UTC, ISO 8601 to
//!   the second.
//! - `SHOWABLE`: when a `thumbnail t` row may be shown, as a condition:
//!   neither of its PNGs is a blocked blob, nor is any file of its
//!   revision. Over the whole tree rather than the one file drawn, which
//!   the row does not record: a takedown shows the placeholder, and the
//!   file a thumbnail drew is among those. Every read of the row carries
//!   this, so the route and the pages agree; one probe of `blocked_blob`
//!   per entry, by primary key.
//! - `LISTED_COLUMNS`: the columns a listing selects, under
//!   `listing::Listed`'s names, from a `design d`, its owner `a` and its
//!   default branch `b`; the thumbnail flag is one probe of `thumbnail`
//!   by its key per row, with `SHOWABLE`'s probes.

/// One `sqlx` query macro over a statement written as a bracketed list of
/// string literals and fragment names (the module's list), with the
/// macro's other arguments as `sqlx` takes them: the record type before
/// the statement, the parameters after it.
macro_rules! statement {
    ($mac:ident!($($record:path,)? [$($piece:tt)+] $($rest:tt)*)) => {
        $crate::sql::statement!(@munch [$mac ($($record,)?) ($($rest)*)] [] $($piece)+)
    };
    (@munch $call:tt [$($done:tt)*] CHAIN $($piece:tt)*) => {
        $crate::sql::statement!(@munch $call [$($done)* "chain AS (
    SELECT b.name, b.head_hash AS hash, 0 AS behind
    FROM branch b WHERE b.design_id = $1 AND ($2::text IS NULL OR b.name = $2)
    UNION ALL
    SELECT c.name, r.parent_hash, c.behind + 1
    FROM chain c JOIN revision r ON r.hash = c.hash WHERE r.parent_hash IS NOT NULL
)"] $($piece)*)
    };
    (@munch $call:tt [$($done:tt)*] DESIGN_COLUMNS $($piece:tt)*) => {
        $crate::sql::statement!(@munch $call [$($done)* r#"d.id AS "id!: crate::design::DesignId",
    d.owner_id AS "owner_id!: crate::account::AccountId",
    d.slug AS "slug!", d.name AS "name!", d.default_branch AS "default_branch!",
    d.visibility AS "visibility!""#] $($piece)*)
    };
    (@munch $call:tt [$($done:tt)*] REVISION_COLUMNS $($piece:tt)*) => {
        $crate::sql::statement!(@munch $call [$($done)* r#"r.hash, r.tree_hash, r.parent_hash, r.author_handle,
    to_char(r.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS "created_at!",
    r.message, r.format_version, r.layer_count, r.copper_layer_count,
    r.board_width_mm, r.board_height_mm, r.fonts, r.net_count, r.footprint_count"#] $($piece)*)
    };
    (@munch $call:tt [$($done:tt)*] SHOWABLE $($piece:tt)*) => {
        $crate::sql::statement!(@munch $call [$($done)* "NOT EXISTS (
        SELECT 1 FROM blocked_blob bb WHERE bb.hash IN (t.large_hash, t.small_hash)
    )
    AND NOT EXISTS (
        SELECT 1 FROM revision r
        JOIN tree_entry te ON te.tree_hash = r.tree_hash
        JOIN blocked_blob bb ON bb.hash = te.blob_hash
        WHERE r.hash = t.revision_hash
    )"] $($piece)*)
    };
    // A fragment with a fragment inside: its pieces go back on the list.
    (@munch $call:tt [$($done:tt)*] LISTED_COLUMNS $($piece:tt)*) => {
        $crate::sql::statement!(@munch $call [$($done)*] r#"a.handle AS "handle!", d.slug, d.name,
    to_char(b.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS "updated_at!",
    encode(b.head_hash, 'hex') AS "head!",
    EXISTS (
        SELECT 1 FROM thumbnail t WHERE t.revision_hash = b.head_hash AND "# SHOWABLE "
    ) AS \"thumbnail!\"" $($piece)*)
    };
    (@munch $call:tt [$($done:tt)*] $text:literal $($piece:tt)*) => {
        $crate::sql::statement!(@munch $call [$($done)* $text] $($piece)*)
    };
    (@munch [$mac:ident ($($record:tt)*) ($($rest:tt)*)] [$first:tt $($done:tt)*]) => {
        ::sqlx::$mac!($($record)* $first $(+ $done)* $($rest)*)
    };
}

pub(crate) use statement;

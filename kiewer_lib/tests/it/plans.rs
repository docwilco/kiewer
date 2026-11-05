//! The plan check (#44): every statement the library runs, as `.sqlx/`
//! records it, is planned against the migrated schema with sequential
//! scans priced out, and none may read a growing table by a Seq Scan.
//! With `enable_seqscan = off` Postgres plans one only when no index can
//! serve the statement, so the check needs no rows. A statement added to
//! `.sqlx/` is planned without any change here; the tests' own statements
//! are runtime queries and stay out of `.sqlx/` (#33), so a setup
//! statement that scans on purpose is never planned.
//!
//! A delete or an update of a table a foreign key references makes
//! Postgres look up the referencing rows, by a query of its own that no
//! `EXPLAIN` of the statement shows. The check plans that lookup too, and
//! follows `ON DELETE CASCADE` to the tables it deletes from in turn: the
//! index behind `revision.author_id` serves no statement but the account
//! delete's `ON DELETE SET NULL`.
//!
//! The tests here run on a database of their own, `DATABASE_URL`'s name
//! plus `_plans` (#107), since one drops an index inside a transaction,
//! which holds the whole table until it rolls back. The Postgres tests
//! skip without `DATABASE_URL`, like `tests/it/db.rs`.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;

use kiewer_assert::assert_is_empty;
use kiewer_test_support::db::pool_of_its_own;
use serde_json::Value;
use sqlx::{PgConnection, Row};
use tokio::sync::Mutex;

/// The tables that grow for the whole retention period
/// (database-conventions, "Tables that grow"): no statement may read one
/// whole, since a scan that is fast today is slow in a year.
const GROWING: &[&str] = &[
    "revision",
    "blob",
    "tree_entry",
    "design_revision",
    "session",
];

/// The statements allowed a Seq Scan, each by its `.sqlx/` file and the
/// growing table it reads whole. The file's name is the hash of the
/// statement's text, so an edited statement is checked afresh, and an
/// entry that matches no scan fails the check, as a stale `#[expect]`
/// does.
const WHOLE_READS: &[(&str, &str)] = &[
    // The collector's revision step (`collector::remove_revisions`): it
    // finds the young revisions it keeps by `created_at`, which has no
    // index, and reads every revision once a night for those nothing
    // keeps. #90 holds whether a nightly whole read stays.
    (
        "query-0cd42dc3542042b98cc35eb2483f6e4db2a2f1ce6a4997b0927bf66f71133eb2.json",
        "revision",
    ),
];

/// The suffix of this module's own database (#107).
const OWN_DATABASE: &str = "plans";

/// The test that drops an index holds `revision` until it rolls back, so
/// the two take turns.
static TURN: Mutex<()> = Mutex::const_new(());

/// One statement of `.sqlx/`: its query file, its text and the sqlx names
/// of its parameters' types.
struct Statement {
    file: String,
    query: String,
    parameters: Vec<String>,
}

/// Every statement under `.sqlx/`, in file order.
fn statements() -> Vec<Statement> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../.sqlx");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("reading {}: {error}", dir.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    files.sort();
    files
        .iter()
        .map(|path| {
            let file = path
                .file_name()
                .expect("a file name")
                .to_string_lossy()
                .into_owned();
            let text = std::fs::read_to_string(path)
                .unwrap_or_else(|error| panic!("reading {file}: {error}"));
            let data: Value =
                serde_json::from_str(&text).unwrap_or_else(|error| panic!("{file}: {error}"));
            let query = data["query"]
                .as_str()
                .unwrap_or_else(|| panic!("{file} has no query"))
                .to_owned();
            let parameters = data["describe"]["parameters"]["Left"]
                .as_array()
                .unwrap_or_else(|| panic!("{file} has no parameter list"))
                .iter()
                .map(|parameter| {
                    parameter
                        .as_str()
                        .unwrap_or_else(|| panic!("{file}: a parameter type {parameter}"))
                        .to_owned()
                })
                .collect();
            Statement {
                file,
                query,
                parameters,
            }
        })
        .collect()
}

/// The Postgres name of a type as sqlx records it: `Int8` is `int8`,
/// `TextArray` is `text[]`.
fn postgres_type(sqlx_name: &str) -> String {
    match sqlx_name.strip_suffix("Array") {
        Some(element) => format!("{}[]", element.to_lowercase()),
        None => sqlx_name.to_lowercase(),
    }
}

/// `items` as a parenthesised list, or nothing when there are none.
fn parenthesised(items: &[impl AsRef<str>]) -> String {
    if items.is_empty() {
        String::new()
    } else {
        let items: Vec<&str> = items.iter().map(AsRef::as_ref).collect();
        format!("({})", items.join(", "))
    }
}

/// The generic plan of `query`, whose parameters have `types`, as
/// `EXPLAIN (FORMAT JSON)` writes it. A NULL of each type stands in for
/// the parameters, and the generic plan is forced: a custom plan would
/// fold `id = NULL` to false and plan no scan at all.
async fn plan(db: &mut PgConnection, query: &str, types: &[String]) -> Value {
    let declared = parenthesised(types);
    // AssertSqlSafe below: `query` is a statement from .sqlx/ and the rest
    // is parameter type names and NULLs the test builds.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "PREPARE planned{declared} AS {query}"
    )))
    .execute(&mut *db)
    .await
    .unwrap_or_else(|error| panic!("preparing {query}: {error}"));
    let nulls = parenthesised(&vec!["NULL"; types.len()]);
    // Not kept prepared: the statement it executes is a new one each time.
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "EXPLAIN (FORMAT JSON) EXECUTE planned{nulls}"
    )))
    .persistent(false)
    .fetch_one(&mut *db)
    .await
    .unwrap_or_else(|error| panic!("planning {query}: {error}"));
    // The column is json, which this crate's sqlx has no type for; json's
    // wire form, binary or text, is its text.
    let text: String = row.try_get_unchecked(0).expect("the plan's text");
    sqlx::raw_sql("DEALLOCATE planned")
        .execute(&mut *db)
        .await
        .expect("deallocate");
    serde_json::from_str(&text).expect("the plan is JSON")
}

/// Every node of a plan, its own and those of the plans under it.
fn nodes(plan: &Value) -> Vec<&Value> {
    let mut found = Vec::new();
    let mut pending: Vec<&Value> = plan
        .as_array()
        .expect("a list of plans")
        .iter()
        .map(|entry| &entry["Plan"])
        .collect();
    while let Some(node) = pending.pop() {
        if let Some(under) = node["Plans"].as_array() {
            pending.extend(under);
        }
        found.push(node);
    }
    found
}

/// The growing tables a plan reads by a Seq Scan.
fn seq_scanned(plan: &Value) -> Vec<String> {
    nodes(plan)
        .into_iter()
        .filter(|node| node["Node Type"] == "Seq Scan")
        .filter_map(|node| node["Relation Name"].as_str())
        .filter(|table| GROWING.contains(table))
        .map(str::to_owned)
        .collect()
}

/// How a statement changes a table whose rows a foreign key references.
#[derive(Clone, Copy)]
enum Change {
    Delete,
    Update,
}

/// The tables a plan deletes from or updates, each with how.
fn modified(plan: &Value) -> Vec<(String, Change)> {
    nodes(plan)
        .into_iter()
        .filter(|node| node["Node Type"] == "ModifyTable")
        .filter_map(|node| {
            let change = match node["Operation"].as_str() {
                Some("Delete") => Change::Delete,
                Some("Update") => Change::Update,
                _ => return None,
            };
            Some((node["Relation Name"].as_str()?.to_owned(), change))
        })
        .collect()
}

/// A foreign key that references a table, with the lookup Postgres makes
/// for the referencing rows when a referenced row goes or changes.
struct Reference {
    name: String,
    referencing: String,
    cascades: bool,
    lookup: String,
    types: Vec<String>,
}

/// The foreign keys that reference `table`.
async fn references(db: &mut PgConnection, table: &str) -> Vec<Reference> {
    sqlx::query(
        "SELECT c.conname::text AS name, c.conrelid::regclass::text AS referencing,
                c.confdeltype = 'c' AS cascades,
                format('SELECT 1 FROM ONLY %s x WHERE %s', c.conrelid::regclass,
                       string_agg(format('x.%I = $%s', a.attname, k.ord), ' AND '
                                  ORDER BY k.ord)) AS lookup,
                array_agg(format_type(a.atttypid, a.atttypmod) ORDER BY k.ord) AS types
         FROM pg_constraint c
         CROSS JOIN LATERAL unnest(c.conkey) WITH ORDINALITY AS k (attnum, ord)
         JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum
         WHERE c.contype = 'f' AND c.confrelid = $1::regclass
         GROUP BY c.oid
         ORDER BY c.conname",
    )
    .bind(table)
    .fetch_all(&mut *db)
    .await
    .expect("the foreign keys")
    .iter()
    .map(|row| Reference {
        name: row.get("name"),
        referencing: row.get("referencing"),
        cascades: row.get("cascades"),
        lookup: row.get("lookup"),
        types: row.get("types"),
    })
    .collect()
}

/// A Seq Scan of a growing table that a statement makes: in its own
/// plan, or in the lookup of a foreign key `through` which Postgres finds
/// the rows that reference a row the statement changes.
struct Scan {
    table: String,
    through: Option<String>,
}

/// The Seq Scans of growing tables that `statement` makes, through `db`
/// with sequential scans priced out.
async fn scans_of(db: &mut PgConnection, statement: &Statement) -> Vec<Scan> {
    let types: Vec<String> = statement
        .parameters
        .iter()
        .map(|name| postgres_type(name))
        .collect();
    let planned = plan(db, &statement.query, &types).await;
    let mut found: Vec<Scan> = seq_scanned(&planned)
        .into_iter()
        .map(|table| Scan {
            table,
            through: None,
        })
        .collect();
    // The lookups of the foreign keys, through every cascade. A
    // constraint's name is unique only within its table.
    let mut seen = HashSet::new();
    let mut pending = modified(&planned);
    while let Some((table, change)) = pending.pop() {
        for reference in references(db, &table).await {
            if !seen.insert((reference.referencing.clone(), reference.name.clone())) {
                continue;
            }
            let lookup = plan(db, &reference.lookup, &reference.types).await;
            found.extend(seq_scanned(&lookup).into_iter().map(|scanned| Scan {
                table: scanned,
                through: Some(format!(
                    "{} ({}) as {table} changes",
                    reference.name, reference.referencing
                )),
            }));
            if matches!(change, Change::Delete) && reference.cascades {
                pending.push((reference.referencing, Change::Delete));
            }
        }
    }
    found
}

/// The lines of the check's report, one finding each. `Debug` prints them
/// one to a line, so `assert_is_empty!` shows each statement on its own.
struct Report(Vec<String>);

impl Report {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for line in &self.0 {
            write!(f, "\n{line}")?;
        }
        Ok(())
    }
}

/// Every Seq Scan of a growing table that a statement of `.sqlx/` makes
/// and [`WHOLE_READS`] does not allow, and every allowance that matches
/// no scan. One test over every statement, not a `#[test_case]` each: the
/// statements are found at run time, and a stale allowance shows only
/// against the whole set.
async fn check(db: &mut PgConnection) -> Report {
    sqlx::raw_sql("SET LOCAL enable_seqscan = off; SET LOCAL plan_cache_mode = force_generic_plan")
        .execute(&mut *db)
        .await
        .expect("set");
    let mut report = Vec::new();
    let mut allowed = HashSet::new();
    for statement in statements() {
        for scan in scans_of(db, &statement).await {
            let allowance = (statement.file.as_str(), scan.table.as_str());
            if scan.through.is_none() && WHOLE_READS.contains(&allowance) {
                allowed.insert((statement.file.clone(), scan.table));
                continue;
            }
            let through = scan
                .through
                .map(|through| format!(" through {through}"))
                .unwrap_or_default();
            report.push(format!(
                "{}: a Seq Scan on {}{through}\n    {}",
                statement.file, scan.table, statement.query
            ));
        }
    }
    for &(file, table) in WHOLE_READS {
        if !allowed.contains(&(file.to_owned(), table.to_owned())) {
            report.push(format!(
                "{file}: allowed a Seq Scan on {table}, which it no longer makes"
            ));
        }
    }
    Report(report)
}

#[tokio::test]
async fn no_statement_reads_a_growing_table_whole() {
    let _turn = TURN.lock().await;
    let Some(db) = pool_of_its_own(OWN_DATABASE).await else {
        return;
    };
    let mut tx = db.begin().await.expect("a transaction");
    assert_is_empty!(check(&mut tx).await);
}

/// The check fails when an index goes: without the one behind
/// `revision.author_id`, deleting an account scans every revision for
/// its own.
#[tokio::test]
async fn a_missing_index_names_the_statement() {
    let _turn = TURN.lock().await;
    let Some(db) = pool_of_its_own(OWN_DATABASE).await else {
        return;
    };
    let mut tx = db.begin().await.expect("a transaction");
    sqlx::raw_sql("DROP INDEX revision_author_id")
        .execute(&mut *tx)
        .await
        .expect("dropped, until the rollback");
    let report = check(&mut tx).await;
    assert!(
        report.0.iter().any(|line| {
            line.contains("a Seq Scan on revision through revision_author_id_fkey")
                && line.contains("DELETE FROM account WHERE id = $1")
        }),
        "{report:?}"
    );
}

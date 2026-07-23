//! Exact v1 SQLite schema creation, attestation, and post-open immutability.

use std::path::Path;

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::sqlite::{LedgerError, SqliteErrorClass};

pub const VERSION: i64 = 1;
pub const MARKER_TABLE: &str = "cc_transcript_schema_v1";

const MARKER_DDL: &str = "
CREATE TABLE cc_transcript_schema_v1 (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  schema_identity TEXT NOT NULL,
  schema_version INTEGER NOT NULL CHECK (schema_version = 1),
  ddl_fingerprint TEXT NOT NULL CHECK (length(ddl_fingerprint) = 64),
  object_fingerprint TEXT NOT NULL CHECK (length(object_fingerprint) = 64)
) STRICT;
";

#[derive(Debug, Clone, PartialEq, Eq)]
struct SchemaObject {
    object_type: String,
    name: String,
    table: String,
    sql: String,
}

pub struct ExactSchema {
    ddl: String,
    objects: Vec<SchemaObject>,
    ddl_fingerprint: String,
    object_fingerprint: String,
}

pub fn error(message: impl Into<String>) -> LedgerError {
    LedgerError::Sqlite {
        class: SqliteErrorClass::Database,
        message: message.into(),
        code: None,
        name: None,
    }
}

pub fn load_extensions(conn: &Connection, paths: &[String]) -> Result<(), LedgerError> {
    if paths.is_empty() {
        return Ok(());
    }
    unsafe { conn.load_extension_enable()? };
    for path in paths {
        if let Err(load_error) = unsafe { conn.load_extension(Path::new(path), None) } {
            let _ = conn.load_extension_disable();
            return Err(load_error.into());
        }
    }
    conn.load_extension_disable()?;
    Ok(())
}

pub fn compile(
    application_ddl: &str,
    extension_paths: &[String],
) -> Result<ExactSchema, LedgerError> {
    let ddl = format!("{MARKER_DDL}{application_ddl}");
    let scratch = Connection::open_in_memory()?;
    scratch.execute_batch("PRAGMA foreign_keys = ON")?;
    load_extensions(&scratch, extension_paths)?;
    scratch.execute_batch(&ddl)?;
    let objects = schema_objects(&scratch)?;
    Ok(ExactSchema {
        ddl_fingerprint: fingerprint(
            b"cc-transcript-sqlite-schema-ddl-v1\0",
            [ddl.as_bytes().to_vec()],
        ),
        object_fingerprint: object_fingerprint(&objects),
        ddl,
        objects,
    })
}

pub fn initialize_or_validate(
    conn: &Connection,
    identity: &str,
    exact: &ExactSchema,
) -> Result<(), LedgerError> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        let version = scalar_i64(conn, "PRAGMA user_version")?;
        let objects = schema_objects(conn)?;
        if version == 0 && objects.is_empty() {
            conn.execute_batch(&exact.ddl)?;
            validate_objects(&schema_objects(conn)?, &exact.objects)?;
            conn.pragma_update(None, "user_version", VERSION)?;
            conn.execute(
                "INSERT INTO cc_transcript_schema_v1 \
                 (id, schema_identity, schema_version, ddl_fingerprint, object_fingerprint) \
                 VALUES (1, ?, 1, ?, ?)",
                params![identity, exact.ddl_fingerprint, exact.object_fingerprint],
            )?;
        }
        validate(conn, identity, exact)
    })();
    match result {
        Ok(()) => conn.execute_batch("COMMIT").map_err(Into::into),
        Err(open_error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(open_error)
        }
    }
}

pub fn validate(conn: &Connection, identity: &str, exact: &ExactSchema) -> Result<(), LedgerError> {
    let version = scalar_i64(conn, "PRAGMA user_version")?;
    if version != VERSION {
        return Err(error(format!(
            "SQLite schema version is {version}, expected exactly {VERSION}"
        )));
    }
    validate_objects(&schema_objects(conn)?, &exact.objects)?;
    let marker = conn
        .query_row(
            "SELECT schema_identity, schema_version, ddl_fingerprint, object_fingerprint \
             FROM cc_transcript_schema_v1 WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((stored_identity, marker_version, ddl_fingerprint, object_fingerprint)) = marker
    else {
        return Err(error("SQLite schema identity row is missing"));
    };
    if stored_identity != identity
        || marker_version != VERSION
        || ddl_fingerprint != exact.ddl_fingerprint
        || object_fingerprint != exact.object_fingerprint
    {
        return Err(error(
            "SQLite schema identity does not match the exact compiled v1 schema",
        ));
    }
    Ok(())
}

pub fn install_guard(conn: &Connection) {
    conn.authorizer(Some(|context: AuthContext<'_>| {
        let changes_schema = matches!(
            context.action,
            AuthAction::CreateIndex { .. }
                | AuthAction::CreateTable { .. }
                | AuthAction::CreateTrigger { .. }
                | AuthAction::CreateView { .. }
                | AuthAction::CreateVtable { .. }
                | AuthAction::DropIndex { .. }
                | AuthAction::DropTable { .. }
                | AuthAction::DropTrigger { .. }
                | AuthAction::DropView { .. }
                | AuthAction::DropVtable { .. }
                | AuthAction::AlterTable { .. }
        );
        let changes_attestation = match context.action {
            AuthAction::Delete { table_name }
            | AuthAction::Insert { table_name }
            | AuthAction::Update { table_name, .. } => {
                table_name.eq_ignore_ascii_case(MARKER_TABLE)
                    || table_name.eq_ignore_ascii_case("sqlite_schema")
                    || table_name.eq_ignore_ascii_case("sqlite_master")
            }
            AuthAction::Pragma {
                pragma_name,
                pragma_value: Some(_),
            } => {
                pragma_name.eq_ignore_ascii_case("user_version")
                    || pragma_name.eq_ignore_ascii_case("writable_schema")
            }
            _ => false,
        };
        if matches!(context.action, AuthAction::Attach { .. })
            || (changes_schema && context.database_name == Some("main"))
            || changes_attestation
        {
            Authorization::Deny
        } else {
            Authorization::Allow
        }
    }));
}

fn schema_objects(conn: &Connection) -> Result<Vec<SchemaObject>, LedgerError> {
    let mut statement = conn.prepare(
        "SELECT type, name, tbl_name, sql FROM sqlite_schema \
         WHERE type IN ('table', 'index', 'trigger', 'view') \
           AND lower(substr(name, 1, 7)) <> 'sqlite_' \
         ORDER BY type COLLATE BINARY, name COLLATE BINARY, tbl_name COLLATE BINARY",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut objects = Vec::new();
    for row in rows {
        let (object_type, name, table, sql) = row?;
        let Some(sql) = sql else {
            return Err(error(format!(
                "SQLite schema {object_type} '{name}' has no definition"
            )));
        };
        objects.push(SchemaObject {
            object_type,
            name,
            table,
            sql,
        });
    }
    Ok(objects)
}

fn fingerprint(domain: &[u8], fields: impl IntoIterator<Item = Vec<u8>>) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    for field in fields {
        digest.update((field.len() as u64).to_be_bytes());
        digest.update(field);
    }
    format!("{:x}", digest.finalize())
}

fn object_fingerprint(objects: &[SchemaObject]) -> String {
    let mut fields = Vec::with_capacity(objects.len() * 4 + 1);
    fields.push((objects.len() as u64).to_be_bytes().to_vec());
    for object in objects {
        fields.extend([
            object.object_type.as_bytes().to_vec(),
            object.name.as_bytes().to_vec(),
            object.table.as_bytes().to_vec(),
            object.sql.as_bytes().to_vec(),
        ]);
    }
    fingerprint(b"cc-transcript-sqlite-schema-objects-v1\0", fields)
}

fn validate_objects(actual: &[SchemaObject], expected: &[SchemaObject]) -> Result<(), LedgerError> {
    if actual.len() != expected.len() {
        return Err(error(format!(
            "SQLite schema has {} objects, expected exactly {}",
            actual.len(),
            expected.len()
        )));
    }
    for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        if actual == expected {
            continue;
        }
        if actual.object_type == expected.object_type && actual.name == expected.name {
            return Err(error(format!(
                "SQLite schema {} '{}' definition differs from exact v1",
                actual.object_type, actual.name
            )));
        }
        return Err(error(format!(
            "SQLite schema object {index} is {} '{}', expected {} '{}'",
            actual.object_type, actual.name, expected.object_type, expected.name
        )));
    }
    Ok(())
}

fn scalar_i64(conn: &Connection, sql: &str) -> Result<i64, LedgerError> {
    Ok(conn.query_row(sql, [], |row| row.get(0))?)
}

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use octra_api_plugin_types::{EntitySpec, MigrationSpec};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::PluginStorageError;
use crate::kv::now_unix;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppliedMigration {
    pub version: String,
    pub checksum: String,
    pub applied_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MigrationReport {
    pub applied: Vec<AppliedMigration>,
    pub skipped: Vec<String>,
}

pub(crate) fn apply(
    conn: &mut Connection,
    artifact_dir: &Path,
    migrations: &[MigrationSpec],
    allow_destructive: bool,
) -> Result<MigrationReport, PluginStorageError> {
    let mut seen = HashSet::new();
    let mut report = MigrationReport::default();

    for migration in migrations {
        if !seen.insert(migration.version.clone()) {
            return Err(PluginStorageError::InvalidManifest(format!(
                "migration version '{}' is declared twice",
                migration.version
            )));
        }

        let path = safe_join(artifact_dir, &migration.file)?;
        let sql = std::fs::read_to_string(&path).map_err(|error| {
            PluginStorageError::InvalidManifest(format!(
                "cannot read migration '{}': {error}",
                migration.file
            ))
        })?;
        let checksum = checksum(&sql);

        if let Some(existing) = ledger_checksum(conn, &migration.version)? {
            if existing == checksum {
                report.skipped.push(migration.version.clone());
                continue;
            }
            return Err(PluginStorageError::ChecksumMismatch {
                version: migration.version.clone(),
            });
        }

        if migration.destructive && !allow_destructive {
            return Err(PluginStorageError::DestructiveMigration {
                version: migration.version.clone(),
            });
        }
        if touches_reserved_tables(&sql) {
            return Err(PluginStorageError::ReservedTable {
                version: migration.version.clone(),
            });
        }

        let applied_at = now_unix();
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction
            .execute_batch(&sql)
            .map_err(|source| PluginStorageError::MigrationFailed {
                version: migration.version.clone(),
                source,
            })?;
        transaction.execute(
            "INSERT INTO _octra_plugin_migrations (version, checksum, applied_at) \
             VALUES (?1, ?2, ?3)",
            params![migration.version, checksum, applied_at],
        )?;
        transaction.commit()?;

        report.applied.push(AppliedMigration {
            version: migration.version.clone(),
            checksum,
            applied_at,
        });
    }

    Ok(report)
}

pub(crate) fn verify_entities<'a>(
    conn: &Connection,
    specs: impl Iterator<Item = &'a EntitySpec>,
) -> Result<(), PluginStorageError> {
    for spec in specs {
        let mut statement =
            conn.prepare(&format!("PRAGMA table_info({})", quote_ident(&spec.name)))?;
        let mut rows = statement.query([])?;

        let mut columns = Vec::new();
        let mut primary = Vec::new();
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            let primary_order: i64 = row.get(5)?;
            if primary_order > 0 {
                primary.push(name.clone());
            }
            columns.push(name);
        }

        if columns.is_empty() {
            return Err(PluginStorageError::EntityMismatch {
                entity: spec.name.clone(),
                fields: vec!["<missing table>".to_string()],
            });
        }

        let missing: Vec<String> = spec
            .fields
            .iter()
            .filter(|field| !columns.contains(&field.name))
            .map(|field| field.name.clone())
            .collect();
        if !missing.is_empty() {
            return Err(PluginStorageError::EntityMismatch {
                entity: spec.name.clone(),
                fields: missing,
            });
        }

        let missing_primary: Vec<String> = spec
            .primary_key
            .iter()
            .filter(|field| !primary.contains(field))
            .cloned()
            .collect();
        if !missing_primary.is_empty() {
            return Err(PluginStorageError::PrimaryKeyMismatch {
                entity: spec.name.clone(),
                fields: missing_primary,
            });
        }
    }
    Ok(())
}

pub(crate) fn ledger(conn: &Connection) -> Result<Vec<AppliedMigration>, PluginStorageError> {
    let mut statement = conn.prepare(
        "SELECT version, checksum, applied_at FROM _octra_plugin_migrations ORDER BY rowid",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(AppliedMigration {
            version: row.get(0)?,
            checksum: row.get(1)?,
            applied_at: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn ledger_checksum(
    conn: &Connection,
    version: &str,
) -> Result<Option<String>, PluginStorageError> {
    Ok(conn
        .query_row(
            "SELECT checksum FROM _octra_plugin_migrations WHERE version = ?1",
            [version],
            |row| row.get(0),
        )
        .optional()?)
}

fn checksum(sql: &str) -> String {
    hex::encode(Sha256::digest(sql.as_bytes()))
}

fn safe_join(base: &Path, relative: &str) -> Result<PathBuf, PluginStorageError> {
    let path = Path::new(relative);
    if path.is_absolute() {
        return Err(PluginStorageError::InvalidManifest(format!(
            "migration path '{}' must be relative",
            relative
        )));
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(PluginStorageError::InvalidManifest(format!(
            "migration path '{}' escapes the artifact directory",
            relative
        )));
    }
    let joined = base.join(path);
    if !joined.starts_with(base) {
        return Err(PluginStorageError::InvalidManifest(format!(
            "migration path '{}' escapes the artifact directory",
            relative
        )));
    }
    Ok(joined)
}

fn touches_reserved_tables(sql: &str) -> bool {
    strip_comments_and_strings(sql)
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .any(|token| token.to_ascii_lowercase().starts_with("_octra_"))
}

fn strip_comments_and_strings(sql: &str) -> String {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Normal,
        LineComment,
        BlockComment,
        SingleQuote,
        DoubleQuote,
    }

    let mut state = State::Normal;
    let mut chars = sql.chars().peekable();
    let mut output = String::with_capacity(sql.len());

    while let Some(character) = chars.next() {
        match state {
            State::Normal => match character {
                '-' if chars.peek() == Some(&'-') => {
                    chars.next();
                    state = State::LineComment;
                }
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    state = State::BlockComment;
                }
                '\'' => {
                    state = State::SingleQuote;
                    output.push(' ');
                }
                '"' => {
                    state = State::DoubleQuote;
                    output.push(' ');
                }
                _ => output.push(character),
            },
            State::LineComment => {
                if character == '\n' {
                    state = State::Normal;
                    output.push('\n');
                }
            }
            State::BlockComment => {
                if character == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    state = State::Normal;
                }
            }
            State::SingleQuote => {
                if character == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        state = State::Normal;
                    }
                }
            }
            State::DoubleQuote => {
                if character == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                    } else {
                        state = State::Normal;
                    }
                }
            }
        }
    }

    output
}

fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_reserved_tables_but_ignores_comments_and_strings() {
        assert!(touches_reserved_tables(
            "CREATE TABLE _octra_plugin_migrations (x INTEGER);"
        ));
        assert!(touches_reserved_tables(
            "CREATE TABLE foo (_octra_kv TEXT);"
        ));
        assert!(!touches_reserved_tables(
            "-- _octra_kv is reserved\nCREATE TABLE foo (x TEXT);"
        ));
        assert!(!touches_reserved_tables(
            "INSERT INTO foo VALUES ('_octra_kv');"
        ));
    }

    #[test]
    fn rejects_path_traversal() {
        let base = Path::new("/tmp/artifact");
        assert!(safe_join(base, "../escape.sql").is_err());
        assert!(safe_join(base, "/etc/passwd").is_err());
        assert!(safe_join(base, "migrations/000_base.sql").is_ok());
    }
}

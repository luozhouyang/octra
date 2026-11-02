use std::time::Duration;

use octra_api_plugin_types::{
    EntityFieldSpec, EntityFilter, EntityOrder, EntityQuery, EntitySpec, FilterOp, MigrationSpec,
    PluginStorage, StorageError,
};
use octra_plugin_storage::{PluginStorageError, PluginStore};
use serde_json::{Value, json};

fn spec() -> EntitySpec {
    EntitySpec {
        name: "quota_usage".to_string(),
        primary_key: vec!["consumer_id".to_string(), "window_start".to_string()],
        fields: vec![
            field("consumer_id", "string"),
            field("window_start", "integer"),
            field("count", "integer"),
            field("updated_at", "integer"),
        ],
        unique: Vec::new(),
    }
}

fn field(name: &str, field_type: &str) -> EntityFieldSpec {
    EntityFieldSpec {
        name: name.to_string(),
        field_type: field_type.to_string(),
        nullable: false,
    }
}

fn migration(version: &str, file: &str) -> MigrationSpec {
    MigrationSpec {
        version: version.to_string(),
        file: file.to_string(),
        destructive: false,
    }
}

fn base_sql() -> &'static str {
    r#"
    CREATE TABLE quota_usage (
        consumer_id  TEXT    NOT NULL,
        window_start INTEGER NOT NULL,
        count        INTEGER NOT NULL DEFAULT 0,
        updated_at   INTEGER NOT NULL,
        PRIMARY KEY (consumer_id, window_start)
    );
    "#
}

fn artifact_with(dir: &std::path::Path, sql: &str) {
    std::fs::create_dir_all(dir.join("migrations")).unwrap();
    std::fs::write(dir.join("migrations/000_base.sql"), sql).unwrap();
}

fn migrated_store() -> (tempfile::TempDir, PluginStore) {
    let dir = tempfile::tempdir().unwrap();
    artifact_with(dir.path(), base_sql());
    let store = PluginStore::open_in_memory("consumer-quota", vec![spec()]).unwrap();
    store
        .apply_migrations(
            dir.path(),
            &[migration("000_base", "migrations/000_base.sql")],
            false,
        )
        .unwrap();
    (dir, store)
}

#[test]
fn applies_and_skips_migrations() {
    let dir = tempfile::tempdir().unwrap();
    artifact_with(dir.path(), base_sql());
    let store = PluginStore::open_in_memory("consumer-quota", vec![spec()]).unwrap();
    let migrations = vec![migration("000_base", "migrations/000_base.sql")];

    let first = store
        .apply_migrations(dir.path(), &migrations, false)
        .unwrap();
    assert_eq!(first.applied.len(), 1);
    assert!(first.skipped.is_empty());

    let second = store
        .apply_migrations(dir.path(), &migrations, false)
        .unwrap();
    assert!(second.applied.is_empty());
    assert_eq!(second.skipped, vec!["000_base".to_string()]);
    assert_eq!(store.ledger().unwrap().len(), 1);
}

#[test]
fn rejects_checksum_changes() {
    let dir = tempfile::tempdir().unwrap();
    artifact_with(dir.path(), base_sql());
    let store = PluginStore::open_in_memory("consumer-quota", vec![spec()]).unwrap();
    let migrations = vec![migration("000_base", "migrations/000_base.sql")];
    store
        .apply_migrations(dir.path(), &migrations, false)
        .unwrap();

    artifact_with(dir.path(), &format!("{}\n-- changed", base_sql()));
    let error = store
        .apply_migrations(dir.path(), &migrations, false)
        .unwrap_err();
    assert!(matches!(
        error,
        PluginStorageError::ChecksumMismatch { .. }
    ));
}

#[test]
fn destructive_migrations_need_approval() {
    let dir = tempfile::tempdir().unwrap();
    artifact_with(dir.path(), base_sql());
    let store = PluginStore::open_in_memory("consumer-quota", vec![spec()]).unwrap();
    let mut spec = migration("000_base", "migrations/000_base.sql");
    spec.destructive = true;

    let error = store.apply_migrations(dir.path(), &[spec], false).unwrap_err();
    assert!(matches!(
        error,
        PluginStorageError::DestructiveMigration { .. }
    ));
}

#[test]
fn rejects_reserved_host_tables() {
    let dir = tempfile::tempdir().unwrap();
    artifact_with(dir.path(), "CREATE TABLE _octra_evil (id TEXT);");
    let store = PluginStore::open_in_memory("consumer-quota", vec![spec()]).unwrap();
    let error = store
        .apply_migrations(
            dir.path(),
            &[migration("000_base", "migrations/000_base.sql")],
            false,
        )
        .unwrap_err();
    assert!(matches!(error, PluginStorageError::ReservedTable { .. }));
}

#[test]
fn rejects_entity_schema_drift() {
    let dir = tempfile::tempdir().unwrap();
    artifact_with(dir.path(), base_sql());
    let mut drift = spec();
    drift.fields.push(field("missing", "string"));
    let store = PluginStore::open_in_memory("consumer-quota", vec![drift]).unwrap();
    let error = store
        .apply_migrations(
            dir.path(),
            &[migration("000_base", "migrations/000_base.sql")],
            false,
        )
        .unwrap_err();
    assert!(matches!(
        error,
        PluginStorageError::EntityMismatch { .. }
    ));
}

#[test]
fn entity_crud_and_query() {
    let (_dir, store) = migrated_store();
    store
        .upsert(
            "quota_usage",
            &json!({
                "consumer_id": "c1",
                "window_start": 60,
                "count": 5,
                "updated_at": 1
            }),
        )
        .unwrap();

    let key = json!({ "consumer_id": "c1", "window_start": 60 });
    let row = store.get("quota_usage", &key).unwrap().unwrap();
    assert_eq!(row["count"], 5);

    let count = store
        .increment("quota_usage", &key, "count", 2, Some(&json!({ "updated_at": 2 })))
        .unwrap();
    assert_eq!(count, 7);
    let row = store.get("quota_usage", &key).unwrap().unwrap();
    assert_eq!(row["count"], 7);
    assert_eq!(row["updated_at"], 2);

    store
        .upsert(
            "quota_usage",
            &json!({
                "consumer_id": "c1",
                "window_start": 120,
                "count": 1,
                "updated_at": 2
            }),
        )
        .unwrap();
    store
        .upsert(
            "quota_usage",
            &json!({
                "consumer_id": "c2",
                "window_start": 60,
                "count": 3,
                "updated_at": 2
            }),
        )
        .unwrap();

    let rows = store
        .query(
            "quota_usage",
            &EntityQuery {
                filters: vec![EntityFilter {
                    field: "consumer_id".to_string(),
                    op: FilterOp::Eq,
                    value: json!("c1"),
                }],
                order_by: vec![EntityOrder {
                    field: "window_start".to_string(),
                    descending: true,
                }],
                limit: Some(1),
                offset: None,
            },
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["window_start"], 120);

    let rows = store
        .query(
            "quota_usage",
            &EntityQuery {
                filters: vec![EntityFilter {
                    field: "window_start".to_string(),
                    op: FilterOp::In,
                    value: json!([60]),
                }],
                order_by: Vec::new(),
                limit: None,
                offset: None,
            },
        )
        .unwrap();
    assert_eq!(rows.len(), 2);

    assert!(store.delete("quota_usage", &key).unwrap());
    assert!(store.get("quota_usage", &key).unwrap().is_none());
}

#[test]
fn rejects_undeclared_entities_and_fields() {
    let store = PluginStore::open_in_memory("consumer-quota", vec![spec()]).unwrap();
    let error = store
        .get("unknown", &json!({ "consumer_id": "c1" }))
        .unwrap_err();
    assert!(matches!(error, StorageError::NotDeclared(_)));

    let error = store
        .upsert("quota_usage", &json!({ "consumer_id": "c1", "window_start": 1, "nope": 1 }))
        .unwrap_err();
    assert!(matches!(error, StorageError::InvalidRequest(_)));
}

#[test]
fn kv_ttl_and_increment() {
    let store = PluginStore::open_in_memory("consumer-quota", vec![spec()]).unwrap();
    store.kv_put("greeting", b"hello", None).unwrap();
    assert_eq!(store.kv_get("greeting").unwrap().unwrap(), b"hello");
    assert!(store.kv_delete("greeting").unwrap());
    assert!(store.kv_get("greeting").unwrap().is_none());

    assert_eq!(store.kv_incr("counter", 2, None).unwrap(), 2);
    assert_eq!(store.kv_incr("counter", 3, None).unwrap(), 5);
    assert_eq!(store.kv_get("counter").unwrap().unwrap(), b"5");

    store
        .kv_put("short", b"lived", Some(Duration::from_secs(0)))
        .unwrap();
    assert!(store.kv_get("short").unwrap().is_none());
}

#[test]
fn returns_rows_with_correct_scalar_types() {
    let (_dir, store) = migrated_store();
    store
        .upsert(
            "quota_usage",
            &json!({
                "consumer_id": "c1",
                "window_start": 1,
                "count": 0,
                "updated_at": 0
            }),
        )
        .unwrap();
    let row: Value = store
        .get(
            "quota_usage",
            &json!({ "consumer_id": "c1", "window_start": 1 }),
        )
        .unwrap()
        .unwrap();
    assert!(row["count"].is_i64());
    assert!(row["consumer_id"].is_string());
}

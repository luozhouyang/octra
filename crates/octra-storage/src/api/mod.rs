//! API gateway repositories: services, routes, upstreams, targets, consumers,
//! consumer groups, plugin instances, credentials, ACLs and policies.

mod acls;
mod consumer_groups;
mod consumers;
mod credentials;
mod plugins;
mod policies;
mod routes;
mod rows;
mod services;
#[cfg(test)]
mod tests;
mod upstreams;

use std::sync::Arc;

use self::rows::*;

use octra_api_core::{
    Acl, BasicauthCredential, Consumer, ConsumerGroup, ConsumerGroupMember, CreateAcl,
    CreateBasicauthCredential, CreateConsumer, CreateConsumerGroup, CreateJwtSecret,
    CreateKeyauthCredential, CreatePluginInstance, CreateRoute, CreateService, CreateTarget,
    CreateUpstream, JwtSecret, KeyauthCredential, LoadBalanceAlgorithm, PluginInstance, Route,
    Service, Target, UpdateAcl, UpdateBasicauthCredential, UpdateConsumer, UpdateConsumerGroup,
    UpdateJwtSecret, UpdateKeyauthCredential, UpdatePluginInstance, UpdateRoute, UpdateService,
    UpdateTarget, UpdateUpstream, Upstream,
};
use octra_core::{Error, Result};
use octra_secrets::SecretBox;
use sqlx::sqlite::SqliteRow;
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool, Transaction};

/// One cursor-keyed page request. `tags` holds ANDed terms, each a set of
/// ORed tag names.
#[derive(Debug, Clone, Default)]
pub struct ListPage {
    pub limit: usize,
    pub after: Option<PageCursor>,
    pub tags: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageCursor {
    pub created_at: i64,
    pub id: String,
}

#[derive(Debug, Clone, Default)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<PageCursor>,
}

const SERVICE_COLUMNS: &str = "id, name, protocol, host, port, path, retries, connect_timeout, \
     write_timeout, read_timeout, enabled, description, tags, created_at, updated_at";
const ROUTE_COLUMNS: &str = "id, name, protocols, methods, hosts, paths, headers, strip_path, \
     preserve_host, regex_priority, service_id, enabled, tags, created_at, updated_at";
const UPSTREAM_COLUMNS: &str = "id, name, algorithm, hash_on, hash_fallback, hash_on_header, \
     hash_fallback_header, hash_on_cookie, hash_on_cookie_path, hash_on_query_arg, \
     hash_fallback_query_arg, hash_on_uri_capture, hash_fallback_uri_capture, slots, \
     healthchecks, host_header, tags, created_at, updated_at";
const TARGET_COLUMNS: &str = "id, upstream_id, target, weight, tags, created_at, updated_at";
const CONSUMER_COLUMNS: &str = "id, username, custom_id, tags, created_at, updated_at";
const PLUGIN_COLUMNS: &str = "id, name, instance_name, config, enabled, protocols, ordering, \
     service_id, route_id, consumer_id, consumer_group_id, tags, created_at, updated_at";
const CONSUMER_GROUP_COLUMNS: &str = "id, name, tags, created_at, updated_at";
const CONSUMER_GROUP_MEMBER_COLUMNS: &str = "consumer_id, consumer_group_id, created_at";
const KEYAUTH_COLUMNS: &str = "id, consumer_id, key, ttl, tags, created_at";
const BASICAUTH_COLUMNS: &str = "id, consumer_id, username, password, tags, created_at";
const JWT_COLUMNS: &str =
    "id, consumer_id, key, secret, algorithm, rsa_public_key, tags, created_at";
const ACL_COLUMNS: &str = "id, consumer_id, \"group\", tags, created_at";

const SQLITE_CONSTRAINT_UNIQUE: &str = "2067";
const SQLITE_CONSTRAINT_PRIMARYKEY: &str = "1555";
const SQLITE_CONSTRAINT_FOREIGNKEY: &str = "787";

/// One coherent view of everything the API data plane needs at runtime.
#[derive(Debug, Clone, Default)]
pub struct ApiRuntimeData {
    pub services: Vec<Service>,
    pub routes: Vec<Route>,
    pub upstreams: Vec<Upstream>,
    pub targets: Vec<Target>,
    pub consumers: Vec<Consumer>,
    pub consumer_groups: Vec<ConsumerGroup>,
    pub consumer_group_members: Vec<ConsumerGroupMember>,
    pub keyauth: Vec<KeyauthCredential>,
    pub basicauth: Vec<BasicauthCredential>,
    pub jwt: Vec<JwtSecret>,
    pub acls: Vec<Acl>,
    pub counts: ApiRuntimeCounts,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiRuntimeCounts {
    pub services: i64,
    pub routes: i64,
    pub upstreams: i64,
    pub targets: i64,
    pub consumers: i64,
    pub credentials: i64,
    pub plugins: i64,
}

fn epoch_now() -> &'static str {
    "CAST(strftime('%s', 'now') AS INTEGER)"
}

/// Maps driver errors to typed storage errors with stable markers so the
/// control plane can translate them into HTTP status codes.
pub fn map_db_error(error: sqlx::Error, entity: &str) -> Error {
    match error {
        sqlx::Error::Database(ref db) if db.is_unique_violation() => match db.code().as_deref() {
            Some(SQLITE_CONSTRAINT_PRIMARYKEY | SQLITE_CONSTRAINT_UNIQUE) => {
                Error::Storage(format!("conflict: {entity} already exists"))
            }
            _ => Error::Storage(format!("conflict: {entity} violates a uniqueness rule")),
        },
        sqlx::Error::Database(ref db)
            if db.code().as_deref() == Some(SQLITE_CONSTRAINT_FOREIGNKEY) =>
        {
            Error::Storage(format!("not_found: {entity} references a missing parent"))
        }
        sqlx::Error::RowNotFound => Error::Storage(format!("not_found: {entity} not found")),
        other => Error::Storage(format!("cannot persist {entity}: {other}")),
    }
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

/// Unwraps a row decode result, mapping failures to storage errors.
pub(crate) fn decode<T>(value: sqlx::error::Result<T>, entity: &str) -> Result<T> {
    value.map_err(|e| map_db_error(e, entity))
}

pub(crate) fn affected(rows: u64, entity: &str) -> Result<()> {
    if rows == 0 {
        Err(Error::NotFound(format!("{entity} not found")))
    } else {
        Ok(())
    }
}

pub(crate) fn encode_json<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|err| Error::Storage(format!("cannot encode JSON payload: {err}")))
}

fn encode_optional_json<T: serde::Serialize>(value: Option<&T>) -> Result<Option<String>> {
    value.map(encode_json).transpose()
}

fn decode_optional_json<T: serde::de::DeserializeOwned>(
    value: Option<String>,
    field: &str,
) -> Result<Option<T>> {
    value
        .map(|raw| {
            serde_json::from_str(&raw)
                .map_err(|err| Error::Storage(format!("invalid {field} JSON '{raw}': {err}")))
        })
        .transpose()
}

fn decode_json_or_default<T: serde::de::DeserializeOwned + Default>(
    value: Option<String>,
    field: &str,
) -> Result<T> {
    Ok(decode_optional_json(value, field)?.unwrap_or_default())
}

pub(crate) use self::rows::map_policy;

fn parse_rfc3339(value: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&chrono::Utc))
        .map_err(|error| Error::Storage(format!("invalid timestamp '{value}': {error}")))
}

fn text_now() -> &'static str {
    "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')"
}

#[derive(Clone)]
pub struct ApiStore {
    pool: SqlitePool,
    secrets: Arc<SecretBox>,
}

impl ApiStore {
    pub(crate) fn new(pool: SqlitePool, secrets: Arc<SecretBox>) -> Self {
        Self { pool, secrets }
    }

    fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub(crate) fn seal_secret(
        &self,
        value: &str,
        table: &str,
        id: &str,
        column: &str,
    ) -> Result<String> {
        if value.is_empty() {
            return Ok(String::new());
        }
        self.secrets
            .seal_with_context(value.as_bytes(), &crate::secret_context(table, id, column))
            .map_err(|_| Error::Storage(format!("cannot encrypt {table}.{column} for row '{id}'")))
    }
}

impl ApiStore {
    /// Loads every entity the API data plane needs in a constant number of
    /// queries (no per-service or per-upstream N+1 reads).
    pub async fn load_runtime_data(&self) -> Result<ApiRuntimeData> {
        let mut tx = self.begin().await?;

        let services =
            fetch_rows::<Service>(&mut tx, "services", SERVICE_COLUMNS, map_service).await?;
        let routes = fetch_rows::<Route>(&mut tx, "routes", ROUTE_COLUMNS, map_route).await?;
        let upstreams =
            fetch_rows::<Upstream>(&mut tx, "upstreams", UPSTREAM_COLUMNS, map_upstream).await?;
        let targets = fetch_rows::<Target>(&mut tx, "targets", TARGET_COLUMNS, map_target).await?;
        let consumers =
            fetch_rows::<Consumer>(&mut tx, "consumers", CONSUMER_COLUMNS, map_consumer).await?;
        let consumer_groups = fetch_rows::<ConsumerGroup>(
            &mut tx,
            "consumer_groups",
            CONSUMER_GROUP_COLUMNS,
            map_consumer_group,
        )
        .await?;
        let member_rows = sqlx::query(&format!(
            "SELECT {CONSUMER_GROUP_MEMBER_COLUMNS} FROM consumer_group_consumers \
             ORDER BY consumer_group_id ASC, consumer_id ASC"
        ))
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| map_db_error(e, "consumer group members"))?;
        let consumer_group_members = member_rows
            .iter()
            .map(map_consumer_group_member)
            .collect::<Result<Vec<_>>>()?;
        let keyauth = fetch_rows::<KeyauthCredential>(
            &mut tx,
            "keyauth_credentials",
            KEYAUTH_COLUMNS,
            |row| map_keyauth(row, &self.secrets),
        )
        .await?;
        let basicauth = fetch_rows::<BasicauthCredential>(
            &mut tx,
            "basicauth_credentials",
            BASICAUTH_COLUMNS,
            |row| map_basicauth(row, &self.secrets),
        )
        .await?;
        let jwt = fetch_rows::<JwtSecret>(&mut tx, "jwt_secrets", JWT_COLUMNS, |row| {
            map_jwt(row, &self.secrets)
        })
        .await?;
        let acls = fetch_rows::<Acl>(&mut tx, "acls", ACL_COLUMNS, map_acl).await?;

        let counts = sqlx::query(
            "SELECT
                (SELECT COUNT(*) FROM services) AS services,
                (SELECT COUNT(*) FROM routes) AS routes,
                (SELECT COUNT(*) FROM upstreams) AS upstreams,
                (SELECT COUNT(*) FROM targets) AS targets,
                (SELECT COUNT(*) FROM consumers) AS consumers,
                (SELECT COUNT(*) FROM consumer_groups) AS consumer_groups,
                (SELECT COUNT(*) FROM keyauth_credentials) AS credentials,
                (SELECT COUNT(*) FROM plugin_instances) AS plugins",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| map_db_error(e, "counts"))?;

        tx.commit()
            .await
            .map_err(|e| map_db_error(e, "runtime data"))?;

        Ok(ApiRuntimeData {
            services,
            routes,
            upstreams,
            targets,
            consumers,
            consumer_groups,
            consumer_group_members,
            keyauth,
            basicauth,
            jwt,
            acls,
            counts: ApiRuntimeCounts {
                services: decode(counts.try_get("services"), "counts")?,
                routes: decode(counts.try_get("routes"), "counts")?,
                upstreams: decode(counts.try_get("upstreams"), "counts")?,
                targets: decode(counts.try_get("targets"), "counts")?,
                consumers: decode(counts.try_get("consumers"), "counts")?,
                credentials: decode(counts.try_get("credentials"), "counts")?,
                plugins: decode(counts.try_get("plugins"), "counts")?,
            },
        })
    }

    pub async fn refresh_runtime_data(&self) -> Result<ApiRuntimeData> {
        self.load_runtime_data().await
    }

    async fn begin(&self) -> Result<Transaction<'_, Sqlite>> {
        self.pool()
            .begin()
            .await
            .map_err(|e| map_db_error(e, "transaction"))
    }

    async fn ensure_parent(&self, table: &str, id: &str, entity: &str) -> Result<()> {
        let exists: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE id = ?"))
            .bind(id)
            .fetch_one(self.pool())
            .await
            .map_err(|e| map_db_error(e, entity))?;
        if exists == 0 {
            return Err(Error::NotFound(format!("{entity} '{id}' not found")));
        }
        Ok(())
    }
}

async fn fetch_rows<T>(
    tx: &mut Transaction<'_, Sqlite>,
    table: &str,
    columns: &str,
    mapper: impl Fn(&SqliteRow) -> Result<T>,
) -> Result<Vec<T>> {
    let rows = sqlx::query(&format!(
        "SELECT {columns} FROM {table} ORDER BY created_at ASC, id ASC"
    ))
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| map_db_error(e, table))?;
    rows.iter().map(mapper).collect()
}

impl ApiStore {
    // The parameters mirror the SQL pagination dimensions; grouping them into
    // a struct would only move the same fields one level down.
    #[allow(clippy::too_many_arguments)]
    async fn page_rows<T>(
        &self,
        table: &str,
        columns: &str,
        parent: Option<(&str, &str)>,
        extra: Option<(&str, &str, &str)>,
        page: &ListPage,
        mapper: impl Fn(&SqliteRow) -> Result<T>,
        cursor_of: impl Fn(&T) -> (i64, String),
    ) -> Result<Page<T>> {
        let limit = page.limit.clamp(1, 1000);
        let mut query =
            QueryBuilder::<Sqlite>::new(format!("SELECT {columns} FROM {table} WHERE 1 = 1"));

        if let Some((column, value)) = parent {
            query.push(format!(" AND {column} = ")).push_bind(value);
        }
        if let Some((before, after, value)) = extra {
            query
                .push(" AND ")
                .push(before)
                .push_bind(value)
                .push(after);
        }
        if let Some(cursor) = &page.after {
            query
                .push(" AND (created_at > ")
                .push_bind(cursor.created_at)
                .push(" OR (created_at = ")
                .push_bind(cursor.created_at)
                .push(" AND id > ")
                .push_bind(cursor.id.clone())
                .push("))");
        }
        for term in &page.tags {
            let term: Vec<&String> = term.iter().filter(|tag| !tag.trim().is_empty()).collect();
            if term.is_empty() {
                continue;
            }
            query
                .push(" AND EXISTS (SELECT 1 FROM json_each(")
                .push(table)
                .push(".tags) WHERE value IN (");
            for (index, tag) in term.iter().enumerate() {
                if index > 0 {
                    query.push(", ");
                }
                query.push_bind((*tag).clone());
            }
            query.push("))");
        }
        query
            .push(" ORDER BY created_at ASC, id ASC LIMIT ")
            .push_bind((limit + 1) as i64);

        let rows = query
            .build()
            .fetch_all(self.pool())
            .await
            .map_err(|e| map_db_error(e, table))?;
        let mut items = rows.iter().map(mapper).collect::<Result<Vec<T>>>()?;
        let has_more = items.len() > limit;
        if has_more {
            items.truncate(limit);
        }
        let next = if has_more {
            items.last().map(|item| {
                let (created_at, id) = cursor_of(item);
                PageCursor { created_at, id }
            })
        } else {
            None
        };
        Ok(Page { items, next })
    }
}

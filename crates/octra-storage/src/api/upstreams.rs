//! Upstream and target repositories.

use super::*;

impl ApiStore {
    // Upstreams

    pub async fn list_upstreams(&self) -> Result<Vec<Upstream>> {
        let rows = sqlx::query(&format!(
            "SELECT {UPSTREAM_COLUMNS} FROM upstreams ORDER BY name ASC, id ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "upstreams"))?;
        rows.iter().map(map_upstream).collect()
    }

    pub async fn get_upstream(&self, id: &str) -> Result<Option<Upstream>> {
        let row = sqlx::query(&format!(
            "SELECT {UPSTREAM_COLUMNS} FROM upstreams WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "upstream"))?;
        row.as_ref().map(map_upstream).transpose()
    }

    pub async fn create_upstream(&self, input: &CreateUpstream) -> Result<Upstream> {
        let id = input.id.clone().unwrap_or_else(|| new_id("up"));
        let algorithm = input
            .algorithm
            .as_deref()
            .map(|value| {
                LoadBalanceAlgorithm::parse(value).ok_or_else(|| {
                    Error::Storage(format!("unknown load balance algorithm '{value}'"))
                })
            })
            .transpose()?
            .unwrap_or_default();

        sqlx::query(&format!(
            "INSERT INTO upstreams (id, name, algorithm, hash_on, hash_fallback, slots, \
             healthchecks, host_header, tags, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, {now}, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.name)
        .bind(algorithm.as_str())
        .bind(input.hash_on.clone().unwrap_or_else(|| "none".into()))
        .bind(input.hash_fallback.clone().unwrap_or_else(|| "none".into()))
        .bind(input.slots.unwrap_or(10_000))
        .bind(encode_json(
            input
                .healthchecks
                .as_ref()
                .unwrap_or(&serde_json::json!({})),
        )?)
        .bind(&input.host_header)
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "upstream"))?;

        self.get_upstream(&id)
            .await?
            .ok_or_else(|| Error::Storage("upstream disappeared after insert".into()))
    }

    pub async fn update_upstream(&self, id: &str, input: &UpdateUpstream) -> Result<Upstream> {
        let algorithm = input
            .algorithm
            .as_deref()
            .map(|value| {
                LoadBalanceAlgorithm::parse(value)
                    .map(LoadBalanceAlgorithm::as_str)
                    .ok_or_else(|| {
                        Error::Storage(format!("unknown load balance algorithm '{value}'"))
                    })
            })
            .transpose()?;

        let result = sqlx::query(&format!(
            "UPDATE upstreams SET \
                name = COALESCE(?, name), \
                algorithm = COALESCE(?, algorithm), \
                hash_on = COALESCE(?, hash_on), \
                hash_fallback = COALESCE(?, hash_fallback), \
                slots = COALESCE(?, slots), \
                healthchecks = COALESCE(?, healthchecks), \
                host_header = COALESCE(?, host_header), \
                tags = COALESCE(?, tags), \
                updated_at = {now} \
             WHERE id = ?",
            now = epoch_now()
        ))
        .bind(&input.name)
        .bind(algorithm)
        .bind(&input.hash_on)
        .bind(&input.hash_fallback)
        .bind(input.slots)
        .bind(input.healthchecks.as_ref().map(encode_json).transpose()?)
        .bind(&input.host_header)
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "upstream"))?;

        affected(result.rows_affected(), "upstream")?;
        self.get_upstream(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: upstream not found".into()))
    }

    pub async fn delete_upstream(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM upstreams WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "upstream"))?;
        affected(result.rows_affected(), "upstream")
    }

    // Targets

    pub async fn list_targets(&self, upstream_id: Option<&str>) -> Result<Vec<Target>> {
        let rows = sqlx::query(&format!(
            "SELECT {TARGET_COLUMNS} FROM targets \
             WHERE (?1 IS NULL OR upstream_id = ?1) ORDER BY created_at ASC, id ASC"
        ))
        .bind(upstream_id)
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "targets"))?;
        rows.iter().map(map_target).collect()
    }

    pub async fn get_target(&self, id: &str) -> Result<Option<Target>> {
        let row = sqlx::query(&format!(
            "SELECT {TARGET_COLUMNS} FROM targets WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "target"))?;
        row.as_ref().map(map_target).transpose()
    }

    pub async fn create_target(&self, input: &CreateTarget) -> Result<Target> {
        self.ensure_parent("upstreams", &input.upstream_id, "upstream")
            .await?;
        let id = input.id.clone().unwrap_or_else(|| new_id("tgt"));
        sqlx::query(&format!(
            "INSERT INTO targets (id, upstream_id, target, weight, tags, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, {now}, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.upstream_id)
        .bind(input.target.trim())
        .bind(input.weight.unwrap_or(100))
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "target"))?;

        self.get_target(&id)
            .await?
            .ok_or_else(|| Error::Storage("target disappeared after insert".into()))
    }

    pub async fn update_target(&self, id: &str, input: &UpdateTarget) -> Result<Target> {
        if let Some(upstream_id) = &input.upstream_id {
            self.ensure_parent("upstreams", upstream_id, "upstream")
                .await?;
        }

        let result = sqlx::query(&format!(
            "UPDATE targets SET \
                upstream_id = COALESCE(?, upstream_id), \
                target = COALESCE(?, target), \
                weight = COALESCE(?, weight), \
                tags = COALESCE(?, tags), \
                updated_at = {now} \
             WHERE id = ?",
            now = epoch_now()
        ))
        .bind(&input.upstream_id)
        .bind(input.target.as_deref().map(str::trim))
        .bind(input.weight)
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "target"))?;

        affected(result.rows_affected(), "target")?;
        self.get_target(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: target not found".into()))
    }

    pub async fn delete_target(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM targets WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "target"))?;
        affected(result.rows_affected(), "target")
    }
}

//! ACL entry repository.

use super::*;

impl ApiStore {
    // ACL entries

    pub async fn list_acls(&self, consumer_id: Option<&str>) -> Result<Vec<Acl>> {
        let rows = sqlx::query(&format!(
            "SELECT {ACL_COLUMNS} FROM acls \
             WHERE (?1 IS NULL OR consumer_id = ?1) ORDER BY \"group\" ASC, id ASC"
        ))
        .bind(consumer_id)
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "acls"))?;
        rows.iter().map(map_acl).collect()
    }

    pub async fn create_acl(&self, input: &CreateAcl) -> Result<Acl> {
        self.ensure_parent("consumers", &input.consumer_id, "consumer")
            .await?;
        let id = input.id.clone().unwrap_or_else(|| new_id("acl"));
        sqlx::query(&format!(
            "INSERT INTO acls (id, consumer_id, \"group\", tags, created_at) \
             VALUES (?, ?, ?, ?, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.consumer_id)
        .bind(&input.group)
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "acl"))?;
        self.get_acl(&id)
            .await?
            .ok_or_else(|| Error::Storage("acl disappeared after insert".into()))
    }

    pub async fn get_acl(&self, id: &str) -> Result<Option<Acl>> {
        let row = sqlx::query(&format!("SELECT {ACL_COLUMNS} FROM acls WHERE id = ?"))
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| map_db_error(e, "acl"))?;
        row.as_ref().map(map_acl).transpose()
    }

    pub async fn update_acl(&self, id: &str, input: &UpdateAcl) -> Result<Acl> {
        let result = sqlx::query(&format!(
            "UPDATE acls SET \"group\" = COALESCE(?, \"group\"), tags = COALESCE(?, tags) \
             WHERE id = ?",
        ))
        .bind(&input.group)
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "acl"))?;
        affected(result.rows_affected(), "acl")?;
        self.get_acl(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: acl not found".into()))
    }

    pub async fn delete_acl(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM acls WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "acl"))?;
        affected(result.rows_affected(), "acl")
    }
}

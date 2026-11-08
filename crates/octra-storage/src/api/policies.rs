//! Policy documents used by the LLM gateway.

use super::*;

impl ApiStore {
    // Policies (LLM authorization documents)

    pub async fn create_policy(
        &self,
        input: &octra_api_core::CreatePolicy,
    ) -> Result<octra_api_core::Policy> {
        let id = input.id.clone().unwrap_or_else(|| new_id("pol"));
        let scope = input.scope.clone().unwrap_or_else(|| {
            if input.consumer_id.is_some() {
                "consumer".into()
            } else {
                "global".into()
            }
        });
        let document = serde_json::to_string(
            input
                .document
                .as_ref()
                .unwrap_or(&serde_json::Value::Object(Default::default())),
        )
        .map_err(|error| Error::Storage(format!("cannot serialize policy document: {error}")))?;
        sqlx::query(&format!(
            "INSERT INTO policies (id, name, scope, consumer_id, document, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, {now}, {now})",
            now = text_now()
        ))
        .bind(&id)
        .bind(&input.name)
        .bind(&scope)
        .bind(&input.consumer_id)
        .bind(document)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "policy"))?;
        self.get_policy(&id)
            .await?
            .ok_or_else(|| Error::Storage("policy disappeared after insert".into()))
    }

    pub async fn get_policy(&self, id: &str) -> Result<Option<octra_api_core::Policy>> {
        let row = sqlx::query(
            "SELECT id, name, scope, consumer_id, document, created_at, updated_at \
             FROM policies WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "policy"))?;
        row.as_ref().map(map_policy).transpose()
    }

    pub async fn update_policy(
        &self,
        id: &str,
        input: &octra_api_core::UpdatePolicy,
    ) -> Result<octra_api_core::Policy> {
        let document = input
            .document
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| {
                Error::Storage(format!("cannot serialize policy document: {error}"))
            })?;
        let result = sqlx::query(&format!(
            "UPDATE policies SET \
                name = COALESCE(?, name), \
                scope = COALESCE(?, scope), \
                consumer_id = COALESCE(?, consumer_id), \
                document = COALESCE(?, document), \
                updated_at = {now} \
             WHERE id = ?",
            now = text_now()
        ))
        .bind(&input.name)
        .bind(&input.scope)
        .bind(&input.consumer_id)
        .bind(document)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "policy"))?;
        affected(result.rows_affected(), "policy")?;
        self.get_policy(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: policy not found".into()))
    }

    pub async fn delete_policy(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM policies WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "policy"))?;
        affected(result.rows_affected(), "policy")
    }

    /// Clears the consumer binding without changing the policy scope. Used
    /// when a policy switches from consumer scope to global scope.
    pub async fn clear_policy_consumer(&self, id: &str) -> Result<()> {
        let result = sqlx::query(&format!(
            "UPDATE policies SET consumer_id = NULL, updated_at = {now} WHERE id = ?",
            now = text_now()
        ))
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "policy"))?;
        affected(result.rows_affected(), "policy")
    }

    pub async fn list_policies(&self) -> Result<Vec<octra_api_core::Policy>> {
        let rows = sqlx::query(
            "SELECT id, name, scope, consumer_id, document, created_at, updated_at \
             FROM policies ORDER BY name ASC, id ASC",
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "policies"))?;
        rows.iter().map(map_policy).collect()
    }
}

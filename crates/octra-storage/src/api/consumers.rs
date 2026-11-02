//! Consumer repository.

use super::*;

impl ApiStore {
    // Consumers

    pub async fn list_consumers_page(&self, page: &ListPage) -> Result<Page<Consumer>> {
        self.page_rows(
            "consumers",
            CONSUMER_COLUMNS,
            None,
            None,
            page,
            map_consumer,
            |consumer| (consumer.created_at, consumer.id.clone()),
        )
        .await
    }

    pub async fn list_consumers(&self) -> Result<Vec<Consumer>> {
        let rows = sqlx::query(&format!(
            "SELECT {CONSUMER_COLUMNS} FROM consumers ORDER BY username ASC, id ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumers"))?;
        rows.iter().map(map_consumer).collect()
    }

    pub async fn get_consumer(&self, id: &str) -> Result<Option<Consumer>> {
        let row = sqlx::query(&format!(
            "SELECT {CONSUMER_COLUMNS} FROM consumers WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer"))?;
        row.as_ref().map(map_consumer).transpose()
    }

    pub async fn create_consumer(&self, input: &CreateConsumer) -> Result<Consumer> {
        let id = input.id.clone().unwrap_or_else(|| new_id("con"));
        sqlx::query(&format!(
            "INSERT INTO consumers (id, username, custom_id, tags, created_at, updated_at) \
             VALUES (?, ?, ?, ?, {now}, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.username)
        .bind(&input.custom_id)
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer"))?;

        self.get_consumer(&id)
            .await?
            .ok_or_else(|| Error::Storage("consumer disappeared after insert".into()))
    }

    pub async fn update_consumer(&self, id: &str, input: &UpdateConsumer) -> Result<Consumer> {
        let result = sqlx::query(&format!(
            "UPDATE consumers SET \
                username = COALESCE(?, username), \
                custom_id = COALESCE(?, custom_id), \
                tags = COALESCE(?, tags), \
                updated_at = {now} \
             WHERE id = ?",
            now = epoch_now()
        ))
        .bind(&input.username)
        .bind(&input.custom_id)
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer"))?;
        affected(result.rows_affected(), "consumer")?;
        self.get_consumer(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: consumer not found".into()))
    }

    pub async fn delete_consumer(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM consumers WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "consumer"))?;
        affected(result.rows_affected(), "consumer")
    }
}

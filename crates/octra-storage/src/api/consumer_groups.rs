//! Consumer group repositories.

use super::*;

impl ApiStore {
    // Consumer groups

    pub async fn list_consumer_groups(&self) -> Result<Vec<ConsumerGroup>> {
        let rows = sqlx::query(&format!(
            "SELECT {CONSUMER_GROUP_COLUMNS} FROM consumer_groups ORDER BY name ASC, id ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer groups"))?;
        rows.iter().map(map_consumer_group).collect()
    }

    pub async fn get_consumer_group(&self, id: &str) -> Result<Option<ConsumerGroup>> {
        let row = sqlx::query(&format!(
            "SELECT {CONSUMER_GROUP_COLUMNS} FROM consumer_groups WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer group"))?;
        row.as_ref().map(map_consumer_group).transpose()
    }

    pub async fn get_consumer_group_by_name(&self, name: &str) -> Result<Option<ConsumerGroup>> {
        let row = sqlx::query(&format!(
            "SELECT {CONSUMER_GROUP_COLUMNS} FROM consumer_groups WHERE name = ?"
        ))
        .bind(name)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer group"))?;
        row.as_ref().map(map_consumer_group).transpose()
    }

    pub async fn create_consumer_group(
        &self,
        input: &CreateConsumerGroup,
    ) -> Result<ConsumerGroup> {
        let id = input.id.clone().unwrap_or_else(|| new_id("cg"));
        sqlx::query(&format!(
            "INSERT INTO consumer_groups (id, name, tags, created_at, updated_at) \
             VALUES (?, ?, ?, {now}, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.name)
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer group"))?;

        self.get_consumer_group(&id)
            .await?
            .ok_or_else(|| Error::Storage("consumer group disappeared after insert".into()))
    }

    pub async fn update_consumer_group(
        &self,
        id: &str,
        input: &UpdateConsumerGroup,
    ) -> Result<ConsumerGroup> {
        let result = sqlx::query(&format!(
            "UPDATE consumer_groups SET \
                name = COALESCE(?, name), \
                tags = COALESCE(?, tags), \
                updated_at = {now} \
             WHERE id = ?",
            now = epoch_now()
        ))
        .bind(&input.name)
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer group"))?;
        affected(result.rows_affected(), "consumer group")?;
        self.get_consumer_group(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: consumer group not found".into()))
    }

    pub async fn delete_consumer_group(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM consumer_groups WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "consumer group"))?;
        affected(result.rows_affected(), "consumer group")
    }

    pub async fn list_consumer_group_members(
        &self,
        consumer_group_id: &str,
    ) -> Result<Vec<Consumer>> {
        let rows = sqlx::query(&format!(
            "SELECT {CONSUMER_COLUMNS} FROM consumers \
             WHERE id IN (SELECT consumer_id FROM consumer_group_consumers \
                          WHERE consumer_group_id = ?) \
             ORDER BY username ASC, id ASC"
        ))
        .bind(consumer_group_id)
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer group members"))?;
        rows.iter().map(map_consumer).collect()
    }

    pub async fn list_consumer_groups_for_consumer(
        &self,
        consumer_id: &str,
    ) -> Result<Vec<ConsumerGroup>> {
        let rows = sqlx::query(&format!(
            "SELECT {CONSUMER_GROUP_COLUMNS} FROM consumer_groups \
             WHERE id IN (SELECT consumer_group_id FROM consumer_group_consumers \
                          WHERE consumer_id = ?) \
             ORDER BY name ASC, id ASC"
        ))
        .bind(consumer_id)
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer groups"))?;
        rows.iter().map(map_consumer_group).collect()
    }

    pub async fn add_consumer_group_member(
        &self,
        consumer_group_id: &str,
        consumer_id: &str,
    ) -> Result<ConsumerGroupMember> {
        self.ensure_parent("consumer_groups", consumer_group_id, "consumer group")
            .await?;
        self.ensure_parent("consumers", consumer_id, "consumer")
            .await?;
        sqlx::query(&format!(
            "INSERT INTO consumer_group_consumers \
                (consumer_id, consumer_group_id, created_at) VALUES (?, ?, {now})",
            now = epoch_now()
        ))
        .bind(consumer_id)
        .bind(consumer_group_id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer group member"))?;

        self.get_consumer_group_member(consumer_group_id, consumer_id)
            .await?
            .ok_or_else(|| Error::Storage("consumer group member disappeared after insert".into()))
    }

    pub async fn get_consumer_group_member(
        &self,
        consumer_group_id: &str,
        consumer_id: &str,
    ) -> Result<Option<ConsumerGroupMember>> {
        let row = sqlx::query(&format!(
            "SELECT {CONSUMER_GROUP_MEMBER_COLUMNS} FROM consumer_group_consumers \
             WHERE consumer_group_id = ? AND consumer_id = ?"
        ))
        .bind(consumer_group_id)
        .bind(consumer_id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer group member"))?;
        row.as_ref().map(map_consumer_group_member).transpose()
    }

    pub async fn remove_consumer_group_member(
        &self,
        consumer_group_id: &str,
        consumer_id: &str,
    ) -> Result<()> {
        let result = sqlx::query(
            "DELETE FROM consumer_group_consumers \
             WHERE consumer_group_id = ? AND consumer_id = ?",
        )
        .bind(consumer_group_id)
        .bind(consumer_id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "consumer group member"))?;
        affected(result.rows_affected(), "consumer group member")
    }
}

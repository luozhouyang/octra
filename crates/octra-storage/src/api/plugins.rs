//! Plugin instance repository.

use super::*;

impl ApiStore {
    // Plugin instances

    pub async fn list_plugin_instances(&self) -> Result<Vec<PluginInstance>> {
        let rows = sqlx::query(&format!(
            "SELECT {PLUGIN_COLUMNS} FROM plugin_instances ORDER BY name ASC, id ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "plugins"))?;
        rows.iter().map(map_plugin).collect()
    }

    pub async fn get_plugin_instance(&self, id: &str) -> Result<Option<PluginInstance>> {
        let row = sqlx::query(&format!(
            "SELECT {PLUGIN_COLUMNS} FROM plugin_instances WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "plugin"))?;
        row.as_ref().map(map_plugin).transpose()
    }

    pub async fn create_plugin_instance(
        &self,
        input: &CreatePluginInstance,
    ) -> Result<PluginInstance> {
        for (table, id, entity) in [
            ("services", input.service_id.as_deref(), "service"),
            ("routes", input.route_id.as_deref(), "route"),
            ("consumers", input.consumer_id.as_deref(), "consumer"),
            (
                "consumer_groups",
                input.consumer_group_id.as_deref(),
                "consumer group",
            ),
        ] {
            if let Some(id) = id {
                self.ensure_parent(table, id, entity).await?;
            }
        }

        let id = input.id.clone().unwrap_or_else(|| new_id("plg"));
        let protocols = input
            .protocols
            .clone()
            .unwrap_or_else(|| vec!["http".into(), "https".into()]);
        sqlx::query(&format!(
            "INSERT INTO plugin_instances (id, name, instance_name, config, enabled, protocols, \
             ordering, service_id, route_id, consumer_id, consumer_group_id, tags, created_at, \
             updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, {now}, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.name)
        .bind(&input.instance_name)
        .bind(encode_json(
            input.config.as_ref().unwrap_or(&serde_json::json!({})),
        )?)
        .bind(input.enabled.unwrap_or(true))
        .bind(encode_json(&protocols)?)
        .bind(input.ordering.as_ref().map(encode_json).transpose()?)
        .bind(&input.service_id)
        .bind(&input.route_id)
        .bind(&input.consumer_id)
        .bind(&input.consumer_group_id)
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "plugin"))?;

        self.get_plugin_instance(&id)
            .await?
            .ok_or_else(|| Error::Storage("plugin disappeared after insert".into()))
    }

    pub async fn update_plugin_instance(
        &self,
        id: &str,
        input: &UpdatePluginInstance,
    ) -> Result<PluginInstance> {
        let service_set = input.service_id.is_some();
        let route_set = input.route_id.is_some();
        let consumer_set = input.consumer_id.is_some();
        let consumer_group_set = input.consumer_group_id.is_some();

        let result = sqlx::query(&format!(
            "UPDATE plugin_instances SET \
                name = COALESCE(?, name), \
                instance_name = COALESCE(?, instance_name), \
                config = COALESCE(?, config), \
                enabled = COALESCE(?, enabled), \
                protocols = COALESCE(?, protocols), \
                ordering = COALESCE(?, ordering), \
                service_id = CASE WHEN ? THEN ? ELSE service_id END, \
                route_id = CASE WHEN ? THEN ? ELSE route_id END, \
                consumer_id = CASE WHEN ? THEN ? ELSE consumer_id END, \
                consumer_group_id = CASE WHEN ? THEN ? ELSE consumer_group_id END, \
                tags = COALESCE(?, tags), \
                updated_at = {now} \
             WHERE id = ?",
            now = epoch_now()
        ))
        .bind(&input.name)
        .bind(&input.instance_name)
        .bind(input.config.as_ref().map(encode_json).transpose()?)
        .bind(input.enabled)
        .bind(input.protocols.as_ref().map(encode_json).transpose()?)
        .bind(input.ordering.as_ref().map(encode_json).transpose()?)
        .bind(service_set)
        .bind(input.service_id.clone().flatten())
        .bind(route_set)
        .bind(input.route_id.clone().flatten())
        .bind(consumer_set)
        .bind(input.consumer_id.clone().flatten())
        .bind(consumer_group_set)
        .bind(input.consumer_group_id.clone().flatten())
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "plugin"))?;

        affected(result.rows_affected(), "plugin")?;
        self.get_plugin_instance(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: plugin not found".into()))
    }

    pub async fn delete_plugin_instance(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM plugin_instances WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "plugin"))?;
        affected(result.rows_affected(), "plugin")
    }
}

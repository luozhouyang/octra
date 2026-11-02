//! Service repository.

use super::*;

impl ApiStore {
    // Services

    pub async fn list_services(&self) -> Result<Vec<Service>> {
        let rows = sqlx::query(&format!(
            "SELECT {SERVICE_COLUMNS} FROM services ORDER BY name ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "services"))?;
        rows.iter().map(map_service).collect()
    }

    pub async fn get_service(&self, id: &str) -> Result<Option<Service>> {
        let row = sqlx::query(&format!(
            "SELECT {SERVICE_COLUMNS} FROM services WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_db_error(e, "service"))?;
        row.as_ref().map(map_service).transpose()
    }

    pub async fn create_service(&self, input: &CreateService) -> Result<Service> {
        let id = input.id.clone().unwrap_or_else(|| new_id("svc"));
        let protocol = input.protocol.clone().unwrap_or_else(|| "http".into());
        let host = input
            .host
            .clone()
            .filter(|host| !host.trim().is_empty())
            .unwrap_or_else(|| input.name.clone());

        sqlx::query(&format!(
            "INSERT INTO services (id, name, protocol, host, port, path, retries, \
             connect_timeout, write_timeout, read_timeout, enabled, description, tags, \
             created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, {now}, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.name)
        .bind(&protocol)
        .bind(&host)
        .bind(input.port.unwrap_or(80))
        .bind(&input.path)
        .bind(input.retries.unwrap_or(5))
        .bind(input.connect_timeout.unwrap_or(60_000))
        .bind(input.write_timeout.unwrap_or(60_000))
        .bind(input.read_timeout.unwrap_or(60_000))
        .bind(input.enabled.unwrap_or(true))
        .bind(input.description.clone().unwrap_or_default())
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "service"))?;

        self.get_service(&id)
            .await?
            .ok_or_else(|| Error::Storage("service disappeared after insert".into()))
    }

    pub async fn update_service(&self, id: &str, input: &UpdateService) -> Result<Service> {
        let path_set = input.path.is_some();
        let path = input.path.clone().flatten();
        let result = sqlx::query(&format!(
            "UPDATE services SET \
                name = COALESCE(?, name), \
                protocol = COALESCE(?, protocol), \
                host = COALESCE(?, host), \
                port = COALESCE(?, port), \
                path = CASE WHEN ? THEN ? ELSE path END, \
                retries = COALESCE(?, retries), \
                connect_timeout = COALESCE(?, connect_timeout), \
                write_timeout = COALESCE(?, write_timeout), \
                read_timeout = COALESCE(?, read_timeout), \
                enabled = COALESCE(?, enabled), \
                description = COALESCE(?, description), \
                tags = COALESCE(?, tags), \
                updated_at = {now} \
             WHERE id = ?",
            now = epoch_now()
        ))
        .bind(&input.name)
        .bind(&input.protocol)
        .bind(&input.host)
        .bind(input.port)
        .bind(path_set)
        .bind(path)
        .bind(input.retries)
        .bind(input.connect_timeout)
        .bind(input.write_timeout)
        .bind(input.read_timeout)
        .bind(input.enabled)
        .bind(&input.description)
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "service"))?;

        affected(result.rows_affected(), "service")?;
        self.get_service(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: service not found".into()))
    }

    pub async fn delete_service(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM services WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "service"))?;
        affected(result.rows_affected(), "service")
    }
}

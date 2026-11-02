//! Route repository.

use super::*;

impl ApiStore {
    // Routes

    pub async fn list_routes_page(
        &self,
        service_id: Option<&str>,
        page: &ListPage,
    ) -> Result<Page<Route>> {
        self.page_rows(
            "routes",
            ROUTE_COLUMNS,
            service_id.map(|id| ("service_id", id)),
            None,
            page,
            map_route,
            |route| (route.created_at, route.id.clone()),
        )
        .await
    }

    pub async fn list_routes(&self, service_id: Option<&str>) -> Result<Vec<Route>> {
        let rows = sqlx::query(&format!(
            "SELECT {ROUTE_COLUMNS} FROM routes WHERE (?1 IS NULL OR service_id = ?1) \
             ORDER BY regex_priority DESC, created_at ASC, id ASC"
        ))
        .bind(service_id)
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_db_error(e, "routes"))?;
        rows.iter().map(map_route).collect()
    }

    pub async fn get_route(&self, id: &str) -> Result<Option<Route>> {
        let row = sqlx::query(&format!("SELECT {ROUTE_COLUMNS} FROM routes WHERE id = ?"))
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| map_db_error(e, "route"))?;
        row.as_ref().map(map_route).transpose()
    }

    pub async fn create_route(&self, input: &CreateRoute) -> Result<Route> {
        if let Some(service_id) = &input.service_id {
            self.ensure_parent("services", service_id, "service")
                .await?;
        }

        let id = input.id.clone().unwrap_or_else(|| new_id("rt"));
        let protocols = input
            .protocols
            .clone()
            .unwrap_or_else(|| vec!["http".into(), "https".into()]);

        sqlx::query(&format!(
            "INSERT INTO routes (id, name, protocols, methods, hosts, paths, headers, \
             strip_path, preserve_host, request_buffering, response_buffering, regex_priority, \
             service_id, enabled, tags, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, {now}, {now})",
            now = epoch_now()
        ))
        .bind(&id)
        .bind(&input.name)
        .bind(encode_json(&protocols)?)
        .bind(encode_optional_json(input.methods.as_ref())?)
        .bind(encode_optional_json(input.hosts.as_ref())?)
        .bind(encode_optional_json(input.paths.as_ref())?)
        .bind(encode_optional_json(input.headers.as_ref())?)
        .bind(input.strip_path.unwrap_or(true))
        .bind(input.preserve_host.unwrap_or(false))
        .bind(input.request_buffering.unwrap_or(true))
        .bind(input.response_buffering.unwrap_or(true))
        .bind(input.regex_priority.unwrap_or(0))
        .bind(&input.service_id)
        .bind(input.enabled.unwrap_or(true))
        .bind(encode_json(&input.tags.clone().unwrap_or_default())?)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "route"))?;

        self.get_route(&id)
            .await?
            .ok_or_else(|| Error::Storage("route disappeared after insert".into()))
    }

    pub async fn update_route(&self, id: &str, input: &UpdateRoute) -> Result<Route> {
        let name_set = input.name.is_some();
        let name = input.name.clone().flatten();
        let service_set = input.service_id.is_some();
        let service_id = input.service_id.clone().flatten();

        let result = sqlx::query(&format!(
            "UPDATE routes SET \
                name = CASE WHEN ? THEN ? ELSE name END, \
                protocols = COALESCE(?, protocols), \
                methods = COALESCE(?, methods), \
                hosts = COALESCE(?, hosts), \
                paths = COALESCE(?, paths), \
                headers = COALESCE(?, headers), \
                strip_path = COALESCE(?, strip_path), \
                preserve_host = COALESCE(?, preserve_host), \
                request_buffering = COALESCE(?, request_buffering), \
                response_buffering = COALESCE(?, response_buffering), \
                regex_priority = COALESCE(?, regex_priority), \
                service_id = CASE WHEN ? THEN ? ELSE service_id END, \
                enabled = COALESCE(?, enabled), \
                tags = COALESCE(?, tags), \
                updated_at = {now} \
             WHERE id = ?",
            now = epoch_now()
        ))
        .bind(name_set)
        .bind(name)
        .bind(input.protocols.as_ref().map(encode_json).transpose()?)
        .bind(input.methods.as_ref().map(encode_json).transpose()?)
        .bind(input.hosts.as_ref().map(encode_json).transpose()?)
        .bind(input.paths.as_ref().map(encode_json).transpose()?)
        .bind(input.headers.as_ref().map(encode_json).transpose()?)
        .bind(input.strip_path)
        .bind(input.preserve_host)
        .bind(input.request_buffering)
        .bind(input.response_buffering)
        .bind(input.regex_priority)
        .bind(service_set)
        .bind(service_id)
        .bind(input.enabled)
        .bind(input.tags.as_ref().map(encode_json).transpose()?)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| map_db_error(e, "route"))?;

        affected(result.rows_affected(), "route")?;
        self.get_route(id)
            .await?
            .ok_or_else(|| Error::Storage("not_found: route not found".into()))
    }

    pub async fn delete_route(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM routes WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| map_db_error(e, "route"))?;
        affected(result.rows_affected(), "route")
    }
}

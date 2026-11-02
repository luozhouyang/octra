use std::collections::HashSet;

use octra_api_plugin_types::{EntityFilter, EntityQuery, EntitySpec, FilterOp};
use rusqlite::types::Value as SqlValue;
use serde_json::Value;
use rusqlite::{Connection, params_from_iter};

use crate::error::PluginStorageError;

const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 1000;

pub(crate) fn validate_spec(spec: &EntitySpec) -> Result<(), PluginStorageError> {
    validate_identifier(&spec.name)?;
    if spec.primary_key.is_empty() {
        return Err(PluginStorageError::InvalidRequest(format!(
            "entity '{}' has no primary key",
            spec.name
        )));
    }
    if spec.fields.is_empty() {
        return Err(PluginStorageError::InvalidRequest(format!(
            "entity '{}' has no fields",
            spec.name
        )));
    }

    let mut fields = HashSet::new();
    for field in &spec.fields {
        validate_identifier(&field.name)?;
        if !matches!(
            field.field_type.as_str(),
            "string" | "integer" | "number" | "boolean" | "bytes"
        ) {
            return Err(PluginStorageError::InvalidRequest(format!(
                "entity '{}' field '{}' has unsupported type '{}'",
                spec.name, field.name, field.field_type
            )));
        }
        if !fields.insert(field.name.as_str()) {
            return Err(PluginStorageError::InvalidRequest(format!(
                "entity '{}' declares field '{}' twice",
                spec.name, field.name
            )));
        }
    }

    let mut primary = HashSet::new();
    for field in &spec.primary_key {
        if !fields.contains(field.as_str()) {
            return Err(PluginStorageError::UnknownField {
                entity: spec.name.clone(),
                field: field.clone(),
            });
        }
        if !primary.insert(field.as_str()) {
            return Err(PluginStorageError::InvalidRequest(format!(
                "entity '{}' lists primary key field '{}' twice",
                spec.name, field
            )));
        }
    }

    for group in &spec.unique {
        if group.is_empty() {
            return Err(PluginStorageError::InvalidRequest(format!(
                "entity '{}' has an empty unique constraint",
                spec.name
            )));
        }
        for field in group {
            if !fields.contains(field.as_str()) {
                return Err(PluginStorageError::UnknownField {
                    entity: spec.name.clone(),
                    field: field.clone(),
                });
            }
        }
    }

    Ok(())
}

pub(crate) fn get(
    conn: &Connection,
    spec: &EntitySpec,
    key: &Value,
) -> Result<Option<Value>, PluginStorageError> {
    let params = key_params(spec, key)?;
    let sql = format!(
        "SELECT {} FROM {} WHERE {}",
        select_fields(spec),
        quote_ident(&spec.name),
        primary_where(&spec.primary_key)
    );
    let mut statement = conn.prepare(&sql)?;
    let mut rows = statement.query(params_from_iter(params))?;
    match rows.next()? {
        Some(row) => Ok(Some(row_to_json(row, spec)?)),
        None => Ok(None),
    }
}

pub(crate) fn upsert(
    conn: &Connection,
    spec: &EntitySpec,
    values: &Value,
) -> Result<(), PluginStorageError> {
    let object = values.as_object().ok_or_else(|| {
        PluginStorageError::InvalidRequest("entity values must be a JSON object".to_string())
    })?;
    validate_object_fields(spec, object.keys().map(String::as_str))?;

    let mut columns = Vec::new();
    let mut params = Vec::new();
    for field in &spec.fields {
        if let Some(value) = object.get(&field.name) {
            columns.push(field.name.as_str());
            params.push(json_to_sql(value)?);
        }
    }
    for field in &spec.primary_key {
        if !object.contains_key(field) {
            return Err(PluginStorageError::InvalidRequest(format!(
                "entity '{}' requires primary key field '{}'",
                spec.name, field
            )));
        }
    }
    if columns.is_empty() {
        return Err(PluginStorageError::InvalidRequest(
            "entity values must not be empty".to_string(),
        ));
    }

    let placeholders = placeholders(1, columns.len());
    let conflict = spec
        .primary_key
        .iter()
        .map(|field| quote_ident(field))
        .collect::<Vec<_>>()
        .join(", ");
    let updates: Vec<String> = spec
        .fields
        .iter()
        .filter(|field| {
            object.contains_key(&field.name) && !spec.primary_key.contains(&field.name)
        })
        .map(|field| {
            format!(
                "{} = excluded.{}",
                quote_ident(&field.name),
                quote_ident(&field.name)
            )
        })
        .collect();

    let suffix = if updates.is_empty() {
        format!("ON CONFLICT({conflict}) DO NOTHING")
    } else {
        format!("ON CONFLICT({conflict}) DO UPDATE SET {}", updates.join(", "))
    };
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({placeholders}) {suffix}",
        quote_ident(&spec.name),
        columns
            .iter()
            .map(|field| quote_ident(field))
            .collect::<Vec<_>>()
            .join(", ")
    );
    conn.execute(&sql, params_from_iter(params))?;
    Ok(())
}

pub(crate) fn increment(
    conn: &Connection,
    spec: &EntitySpec,
    key: &Value,
    field: &str,
    delta: i64,
    extra: Option<&Value>,
) -> Result<i64, PluginStorageError> {
    let field_spec = spec
        .fields
        .iter()
        .find(|candidate| candidate.name == field)
        .ok_or_else(|| PluginStorageError::UnknownField {
            entity: spec.name.clone(),
            field: field.to_string(),
        })?;
    if !matches!(field_spec.field_type.as_str(), "integer" | "number") {
        return Err(PluginStorageError::InvalidRequest(format!(
            "entity '{}' field '{}' is not numeric",
            spec.name, field
        )));
    }
    if spec.primary_key.iter().any(|pk| pk == field) {
        return Err(PluginStorageError::InvalidRequest(
            "cannot increment a primary key field".to_string(),
        ));
    }

    let key_values = key_object(spec, key)?;
    let extra_object = match extra {
        Some(value) => {
            let object = value.as_object().ok_or_else(|| {
                PluginStorageError::InvalidRequest(
                    "extra entity values must be a JSON object".to_string(),
                )
            })?;
            validate_object_fields(spec, object.keys().map(String::as_str))?;
            for name in object.keys() {
                if spec.primary_key.contains(name) || name == field {
                    return Err(PluginStorageError::InvalidRequest(format!(
                        "extra values must not set primary key or incremented field '{name}'"
                    )));
                }
            }
            Some(object)
        }
        None => None,
    };

    let mut columns = Vec::new();
    let mut params = Vec::new();
    for pk in &spec.primary_key {
        columns.push(pk.as_str());
        let value = key_values.get(pk).expect("validated primary key");
        params.push(json_to_sql(value)?);
    }
    columns.push(field);
    params.push(SqlValue::Integer(delta));
    if let Some(object) = &extra_object {
        for field_spec in &spec.fields {
            if let Some(value) = object.get(&field_spec.name) {
                columns.push(field_spec.name.as_str());
                params.push(json_to_sql(value)?);
            }
        }
    }

    let placeholders = placeholders(1, columns.len());
    let conflict = spec
        .primary_key
        .iter()
        .map(|pk| quote_ident(pk))
        .collect::<Vec<_>>()
        .join(", ");
    let mut updates = vec![format!(
        "{} = {}.{} + excluded.{}",
        quote_ident(field),
        quote_ident(&spec.name),
        quote_ident(field),
        quote_ident(field)
    )];
    if let Some(object) = &extra_object {
        for field_spec in &spec.fields {
            if object.contains_key(&field_spec.name) {
                updates.push(format!(
                    "{} = excluded.{}",
                    quote_ident(&field_spec.name),
                    quote_ident(&field_spec.name)
                ));
            }
        }
    }

    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({placeholders}) \
         ON CONFLICT({conflict}) DO UPDATE SET {} \
         RETURNING {}",
        quote_ident(&spec.name),
        columns
            .iter()
            .map(|column| quote_ident(column))
            .collect::<Vec<_>>()
            .join(", "),
        updates.join(", "),
        quote_ident(field)
    );
    let value = conn.query_row(&sql, params_from_iter(params), |row| {
        row.get::<_, i64>(0)
    })?;
    Ok(value)
}

pub(crate) fn delete(
    conn: &Connection,
    spec: &EntitySpec,
    key: &Value,
) -> Result<bool, PluginStorageError> {
    let params = key_params(spec, key)?;
    let sql = format!(
        "DELETE FROM {} WHERE {}",
        quote_ident(&spec.name),
        primary_where(&spec.primary_key)
    );
    let changed = conn.execute(&sql, params_from_iter(params))?;
    Ok(changed > 0)
}

pub(crate) fn query(
    conn: &Connection,
    spec: &EntitySpec,
    query: &EntityQuery,
) -> Result<Vec<Value>, PluginStorageError> {
    let mut clauses = Vec::new();
    let mut params = Vec::new();

    for filter in &query.filters {
        append_filter(spec, filter, &mut clauses, &mut params)?;
    }

    let mut sql = format!("SELECT {} FROM {}", select_fields(spec), quote_ident(&spec.name));
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }

    if !query.order_by.is_empty() {
        let mut order = Vec::new();
        for item in &query.order_by {
            ensure_field(spec, &item.field)?;
            order.push(format!(
                "{} {}",
                quote_ident(&item.field),
                if item.descending { "DESC" } else { "ASC" }
            ));
        }
        sql.push_str(" ORDER BY ");
        sql.push_str(&order.join(", "));
    }

    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = query.offset.unwrap_or(0);
    let limit_index = params.len() + 1;
    let offset_index = params.len() + 2;
    sql.push_str(&format!(" LIMIT ?{limit_index} OFFSET ?{offset_index}"));
    params.push(SqlValue::Integer(i64::from(limit)));
    params.push(SqlValue::Integer(i64::from(offset)));

    let mut statement = conn.prepare(&sql)?;
    let mut rows = statement.query(params_from_iter(params))?;
    let mut values = Vec::new();
    while let Some(row) = rows.next()? {
        values.push(row_to_json(row, spec)?);
    }
    Ok(values)
}

fn append_filter(
    spec: &EntitySpec,
    filter: &EntityFilter,
    clauses: &mut Vec<String>,
    params: &mut Vec<SqlValue>,
) -> Result<(), PluginStorageError> {
    ensure_field(spec, &filter.field)?;
    let column = quote_ident(&filter.field);

    if filter.op == FilterOp::In {
        let values = filter.value.as_array().ok_or_else(|| {
            PluginStorageError::InvalidRequest(format!(
                "filter on '{}' with 'in' requires a JSON array",
                filter.field
            ))
        })?;
        if values.is_empty() {
            clauses.push("0 = 1".to_string());
            return Ok(());
        }
        let start = params.len() + 1;
        let placeholders = placeholders(start, values.len());
        for value in values {
            params.push(json_to_sql(value)?);
        }
        clauses.push(format!("{column} IN ({placeholders})"));
        return Ok(());
    }

    let operator = match filter.op {
        FilterOp::Eq => "=",
        FilterOp::Ne => "!=",
        FilterOp::Lt => "<",
        FilterOp::Lte => "<=",
        FilterOp::Gt => ">",
        FilterOp::Gte => ">=",
        FilterOp::In => unreachable!(),
    };
    let index = params.len() + 1;
    params.push(json_to_sql(&filter.value)?);
    clauses.push(format!("{column} {operator} ?{index}"));
    Ok(())
}

fn key_params(spec: &EntitySpec, key: &Value) -> Result<Vec<SqlValue>, PluginStorageError> {
    let object = key_object(spec, key)?;
    spec.primary_key
        .iter()
        .map(|field| json_to_sql(object.get(field).expect("validated primary key")))
        .collect()
}

fn key_object<'a>(
    spec: &EntitySpec,
    key: &'a Value,
) -> Result<&'a serde_json::Map<String, Value>, PluginStorageError> {
    let object = key.as_object().ok_or_else(|| {
        PluginStorageError::InvalidRequest("entity key must be a JSON object".to_string())
    })?;
    validate_object_fields(spec, object.keys().map(String::as_str))?;
    for field in &spec.primary_key {
        if !object.contains_key(field) {
            return Err(PluginStorageError::InvalidRequest(format!(
                "entity '{}' key requires field '{}'",
                spec.name, field
            )));
        }
    }
    Ok(object)
}

fn validate_object_fields<'a>(
    spec: &EntitySpec,
    names: impl Iterator<Item = &'a str>,
) -> Result<(), PluginStorageError> {
    for name in names {
        ensure_field(spec, name)?;
    }
    Ok(())
}

fn ensure_field(spec: &EntitySpec, field: &str) -> Result<(), PluginStorageError> {
    if spec.fields.iter().any(|candidate| candidate.name == field) {
        Ok(())
    } else {
        Err(PluginStorageError::UnknownField {
            entity: spec.name.clone(),
            field: field.to_string(),
        })
    }
}

fn select_fields(spec: &EntitySpec) -> String {
    spec.fields
        .iter()
        .map(|field| quote_ident(&field.name))
        .collect::<Vec<_>>()
        .join(", ")
}

fn primary_where(primary_key: &[String]) -> String {
    primary_key
        .iter()
        .enumerate()
        .map(|(index, field)| format!("{} = ?{}", quote_ident(field), index + 1))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn placeholders(start: usize, count: usize) -> String {
    (start..start + count)
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn validate_identifier(value: &str) -> Result<(), PluginStorageError> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(PluginStorageError::InvalidRequest(
            "empty identifier".to_string(),
        ));
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(PluginStorageError::InvalidRequest(format!(
            "invalid identifier '{value}'"
        )));
    }
    if !chars.all(|character| character.is_ascii_alphanumeric() || character == '_') {
        return Err(PluginStorageError::InvalidRequest(format!(
            "invalid identifier '{value}'"
        )));
    }
    Ok(())
}

fn json_to_sql(value: &Value) -> Result<SqlValue, PluginStorageError> {
    match value {
        Value::Null => Ok(SqlValue::Null),
        Value::Bool(value) => Ok(SqlValue::Integer(i64::from(*value))),
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                Ok(SqlValue::Integer(value))
            } else if let Some(value) = number.as_u64() {
                if value <= i64::MAX as u64 {
                    Ok(SqlValue::Integer(value as i64))
                } else {
                    Ok(SqlValue::Real(value as f64))
                }
            } else if let Some(value) = number.as_f64() {
                Ok(SqlValue::Real(value))
            } else {
                Err(PluginStorageError::InvalidRequest(
                    "unsupported JSON number".to_string(),
                ))
            }
        }
        Value::String(value) => Ok(SqlValue::Text(value.clone())),
        Value::Array(_) | Value::Object(_) => Err(PluginStorageError::InvalidRequest(
            "entity values must be scalar".to_string(),
        )),
    }
}

fn row_to_json(
    row: &rusqlite::Row<'_>,
    spec: &EntitySpec,
) -> Result<Value, PluginStorageError> {
    let mut object = serde_json::Map::new();
    for (index, field) in spec.fields.iter().enumerate() {
        let value: SqlValue = row.get(index)?;
        object.insert(field.name.clone(), sql_to_json(value, &field.field_type));
    }
    Ok(Value::Object(object))
}

fn sql_to_json(value: SqlValue, field_type: &str) -> Value {
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(value) => {
            if field_type == "boolean" {
                Value::Bool(value != 0)
            } else {
                Value::Number(value.into())
            }
        }
        SqlValue::Real(value) => serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        SqlValue::Text(value) => Value::String(value),
        SqlValue::Blob(value) => {
            if field_type == "bytes" {
                Value::Array(
                    value
                        .into_iter()
                        .map(|byte| Value::Number(byte.into()))
                        .collect(),
                )
            } else {
                Value::String(String::from_utf8_lossy(&value).into_owned())
            }
        }
    }
}

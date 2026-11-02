//! `/admin/key-auth`, `/admin/basic-auth`, `/admin/jwt`, and `/admin/acls`.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use octra_api_core::{
    CreateAcl, CreateBasicauthCredential, CreateJwtSecret, CreateKeyauthCredential, UpdateAcl,
    UpdateBasicauthCredential, UpdateJwtSecret, UpdateKeyauthCredential,
};
use serde_json::{Value, json};

use crate::ApiControlState;
use crate::key::generate_api_key;

use super::error::{AdminError, AdminResult};
use super::list::{
    ListQuery, ListResponse, deserialize, list_page, list_response, normalize_fk, read_json_body,
    set_id, to_value, with_ref,
};
use super::{reload_all, require_consumer};

// Key-auth

pub async fn list_keyauth(
    State(state): State<ApiControlState>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    let page = state
        .storage
        .api
        .list_credentials_page(None, &list_page(&query))
        .await?;
    let next = page.next;
    let items = page
        .items
        .iter()
        .map(keyauth_view)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response("/admin/key-auths", items, next, &query))
}

pub async fn list_keyauth_for_consumer(
    State(state): State<ApiControlState>,
    Path(consumer_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    require_consumer(&state, &consumer_id).await?;
    let page = state
        .storage
        .api
        .list_credentials_page(Some(&consumer_id), &list_page(&query))
        .await?;
    let next = page.next;
    let items = page
        .items
        .iter()
        .map(keyauth_view)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response(
        &format!("/admin/consumers/{consumer_id}/key-auth"),
        items,
        next,
        &query,
    ))
}

pub async fn get_keyauth(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<Json<Value>> {
    let credential = state
        .storage
        .api
        .get_credential(&id)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("credential '{id}' not found")))?;
    Ok(Json(keyauth_view(&credential)?))
}

pub async fn create_keyauth(
    State(state): State<ApiControlState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "consumer", "consumer_id");
    let mut input: CreateKeyauthCredential = deserialize(body)?;
    if input.key.is_none() {
        input.key = Some(generate_api_key().plaintext);
    }
    input.validate()?;
    let credential = state.storage.api.create_credential(&input).await?;
    reload_all(&state).await?;
    Ok((StatusCode::CREATED, Json(keyauth_view(&credential)?)))
}

pub async fn create_keyauth_for_consumer(
    State(state): State<ApiControlState>,
    Path(consumer_id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    require_consumer(&state, &consumer_id).await?;
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "consumer", "consumer_id");
    if let Some(object) = body.as_object_mut() {
        object.insert("consumer_id".into(), json!(consumer_id));
    }
    let mut input: CreateKeyauthCredential = deserialize(body)?;
    if input.key.is_none() {
        input.key = Some(generate_api_key().plaintext);
    }
    input.validate()?;
    let credential = state.storage.api.create_credential(&input).await?;
    reload_all(&state).await?;
    Ok((StatusCode::CREATED, Json(keyauth_view(&credential)?)))
}

pub async fn update_keyauth(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let input: UpdateKeyauthCredential = deserialize(read_json_body(payload)?)?;
    input.validate()?;
    let credential = state.storage.api.update_credential(&id, &input).await?;
    reload_all(&state).await?;
    Ok(Json(keyauth_view(&credential)?))
}

pub async fn upsert_keyauth(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "consumer", "consumer_id");
    let credential = if state.storage.api.get_credential(&id).await?.is_some() {
        let input: UpdateKeyauthCredential = deserialize(body)?;
        input.validate()?;
        state.storage.api.update_credential(&id, &input).await?
    } else {
        set_id(&mut body, &id);
        let mut input: CreateKeyauthCredential = deserialize(body)?;
        if input.key.is_none() {
            input.key = Some(generate_api_key().plaintext);
        }
        input.validate()?;
        state.storage.api.create_credential(&input).await?
    };
    reload_all(&state).await?;
    Ok(Json(keyauth_view(&credential)?))
}

pub async fn delete_keyauth(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<StatusCode> {
    state.storage.api.delete_credential(&id).await?;
    reload_all(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// Basic auth

pub async fn list_basicauth(
    State(state): State<ApiControlState>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    let page = state
        .storage
        .api
        .list_basicauth_page(None, &list_page(&query))
        .await?;
    let next = page.next;
    let items = page
        .items
        .iter()
        .map(basicauth_view)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response("/admin/basic-auths", items, next, &query))
}

pub async fn list_basicauth_for_consumer(
    State(state): State<ApiControlState>,
    Path(consumer_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    require_consumer(&state, &consumer_id).await?;
    let page = state
        .storage
        .api
        .list_basicauth_page(Some(&consumer_id), &list_page(&query))
        .await?;
    let next = page.next;
    let items = page
        .items
        .iter()
        .map(basicauth_view)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response(
        &format!("/admin/consumers/{consumer_id}/basic-auth"),
        items,
        next,
        &query,
    ))
}

pub async fn get_basicauth(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<Json<Value>> {
    let credential = state
        .storage
        .api
        .get_basicauth(&id)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("credential '{id}' not found")))?;
    Ok(Json(basicauth_view(&credential)?))
}

pub async fn create_basicauth(
    State(state): State<ApiControlState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "consumer", "consumer_id");
    let mut input: CreateBasicauthCredential = deserialize(body)?;
    input.validate()?;
    input.password = hash_password(input.password).await?;
    let credential = state.storage.api.create_basicauth(&input).await?;
    reload_all(&state).await?;
    Ok((StatusCode::CREATED, Json(basicauth_view(&credential)?)))
}

pub async fn create_basicauth_for_consumer(
    State(state): State<ApiControlState>,
    Path(consumer_id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    require_consumer(&state, &consumer_id).await?;
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "consumer", "consumer_id");
    if let Some(object) = body.as_object_mut() {
        object.insert("consumer_id".into(), json!(consumer_id));
    }
    let mut input: CreateBasicauthCredential = deserialize(body)?;
    input.validate()?;
    input.password = hash_password(input.password).await?;
    let credential = state.storage.api.create_basicauth(&input).await?;
    reload_all(&state).await?;
    Ok((StatusCode::CREATED, Json(basicauth_view(&credential)?)))
}

pub async fn update_basicauth(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let mut input: UpdateBasicauthCredential = deserialize(read_json_body(payload)?)?;
    input.validate()?;
    if let Some(password) = input.password.take() {
        input.password = Some(hash_password(password).await?);
    }
    let credential = state.storage.api.update_basicauth(&id, &input).await?;
    reload_all(&state).await?;
    Ok(Json(basicauth_view(&credential)?))
}

pub async fn upsert_basicauth(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "consumer", "consumer_id");
    let credential = if state.storage.api.get_basicauth(&id).await?.is_some() {
        let mut input: UpdateBasicauthCredential = deserialize(body)?;
        input.validate()?;
        if let Some(password) = input.password.take() {
            input.password = Some(hash_password(password).await?);
        }
        state.storage.api.update_basicauth(&id, &input).await?
    } else {
        set_id(&mut body, &id);
        let mut input: CreateBasicauthCredential = deserialize(body)?;
        input.validate()?;
        input.password = hash_password(input.password).await?;
        state.storage.api.create_basicauth(&input).await?
    };
    reload_all(&state).await?;
    Ok(Json(basicauth_view(&credential)?))
}

pub async fn delete_basicauth(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<StatusCode> {
    state.storage.api.delete_basicauth(&id).await?;
    reload_all(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// JWT

pub async fn list_jwt(
    State(state): State<ApiControlState>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    let page = state
        .storage
        .api
        .list_jwt_page(None, &list_page(&query))
        .await?;
    let next = page.next;
    let items = page
        .items
        .iter()
        .map(jwt_view)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response("/admin/jwts", items, next, &query))
}

pub async fn list_jwt_for_consumer(
    State(state): State<ApiControlState>,
    Path(consumer_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    require_consumer(&state, &consumer_id).await?;
    let page = state
        .storage
        .api
        .list_jwt_page(Some(&consumer_id), &list_page(&query))
        .await?;
    let next = page.next;
    let items = page
        .items
        .iter()
        .map(jwt_view)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response(
        &format!("/admin/consumers/{consumer_id}/jwt"),
        items,
        next,
        &query,
    ))
}

pub async fn get_jwt(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<Json<Value>> {
    let credential = state
        .storage
        .api
        .get_jwt(&id)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("credential '{id}' not found")))?;
    Ok(Json(jwt_view(&credential)?))
}

pub async fn create_jwt(
    State(state): State<ApiControlState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "consumer", "consumer_id");
    let input: CreateJwtSecret = deserialize(body)?;
    input.validate()?;
    let credential = state.storage.api.create_jwt(&input).await?;
    reload_all(&state).await?;
    Ok((StatusCode::CREATED, Json(jwt_view(&credential)?)))
}

pub async fn create_jwt_for_consumer(
    State(state): State<ApiControlState>,
    Path(consumer_id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    require_consumer(&state, &consumer_id).await?;
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "consumer", "consumer_id");
    if let Some(object) = body.as_object_mut() {
        object.insert("consumer_id".into(), json!(consumer_id));
    }
    let input: CreateJwtSecret = deserialize(body)?;
    input.validate()?;
    let credential = state.storage.api.create_jwt(&input).await?;
    reload_all(&state).await?;
    Ok((StatusCode::CREATED, Json(jwt_view(&credential)?)))
}

pub async fn update_jwt(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let input: UpdateJwtSecret = deserialize(read_json_body(payload)?)?;
    input.validate()?;
    let credential = state.storage.api.update_jwt(&id, &input).await?;
    reload_all(&state).await?;
    Ok(Json(jwt_view(&credential)?))
}

pub async fn upsert_jwt(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "consumer", "consumer_id");
    let credential = if state.storage.api.get_jwt(&id).await?.is_some() {
        let input: UpdateJwtSecret = deserialize(body)?;
        input.validate()?;
        state.storage.api.update_jwt(&id, &input).await?
    } else {
        set_id(&mut body, &id);
        let input: CreateJwtSecret = deserialize(body)?;
        input.validate()?;
        state.storage.api.create_jwt(&input).await?
    };
    reload_all(&state).await?;
    Ok(Json(jwt_view(&credential)?))
}

pub async fn delete_jwt(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<StatusCode> {
    state.storage.api.delete_jwt(&id).await?;
    reload_all(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ACLs

pub async fn list_acls(
    State(state): State<ApiControlState>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    let page = state
        .storage
        .api
        .list_acls_page(None, &list_page(&query))
        .await?;
    let next = page.next;
    let items = page
        .items
        .iter()
        .map(acl_view)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response("/admin/acls", items, next, &query))
}

pub async fn list_acls_for_consumer(
    State(state): State<ApiControlState>,
    Path(consumer_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    require_consumer(&state, &consumer_id).await?;
    let page = state
        .storage
        .api
        .list_acls_page(Some(&consumer_id), &list_page(&query))
        .await?;
    let next = page.next;
    let items = page
        .items
        .iter()
        .map(acl_view)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response(
        &format!("/admin/consumers/{consumer_id}/acls"),
        items,
        next,
        &query,
    ))
}

pub async fn get_acl(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<Json<Value>> {
    let acl = state
        .storage
        .api
        .get_acl(&id)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("acl '{id}' not found")))?;
    Ok(Json(acl_view(&acl)?))
}

pub async fn create_acl(
    State(state): State<ApiControlState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "consumer", "consumer_id");
    let input: CreateAcl = deserialize(body)?;
    input.validate()?;
    let acl = state.storage.api.create_acl(&input).await?;
    reload_all(&state).await?;
    Ok((StatusCode::CREATED, Json(acl_view(&acl)?)))
}

pub async fn create_acl_for_consumer(
    State(state): State<ApiControlState>,
    Path(consumer_id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    require_consumer(&state, &consumer_id).await?;
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "consumer", "consumer_id");
    if let Some(object) = body.as_object_mut() {
        object.insert("consumer_id".into(), json!(consumer_id));
    }
    let input: CreateAcl = deserialize(body)?;
    input.validate()?;
    let acl = state.storage.api.create_acl(&input).await?;
    reload_all(&state).await?;
    Ok((StatusCode::CREATED, Json(acl_view(&acl)?)))
}

pub async fn update_acl(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let input: UpdateAcl = deserialize(read_json_body(payload)?)?;
    input.validate()?;
    let acl = state.storage.api.update_acl(&id, &input).await?;
    reload_all(&state).await?;
    Ok(Json(acl_view(&acl)?))
}

pub async fn upsert_acl(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "consumer", "consumer_id");
    let acl = if state.storage.api.get_acl(&id).await?.is_some() {
        let input: UpdateAcl = deserialize(body)?;
        input.validate()?;
        state.storage.api.update_acl(&id, &input).await?
    } else {
        set_id(&mut body, &id);
        let input: CreateAcl = deserialize(body)?;
        input.validate()?;
        state.storage.api.create_acl(&input).await?
    };
    reload_all(&state).await?;
    Ok(Json(acl_view(&acl)?))
}

pub async fn delete_acl(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<StatusCode> {
    state.storage.api.delete_acl(&id).await?;
    reload_all(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn keyauth_view(credential: &octra_api_core::KeyauthCredential) -> AdminResult<Value> {
    let mut value = to_value(credential)?;
    with_ref(
        &mut value,
        "consumer",
        Some(credential.consumer_id.as_str()),
    );
    Ok(value)
}

fn basicauth_view(credential: &octra_api_core::BasicauthCredential) -> AdminResult<Value> {
    let mut value = to_value(credential)?;
    with_ref(
        &mut value,
        "consumer",
        Some(credential.consumer_id.as_str()),
    );
    Ok(value)
}

fn jwt_view(credential: &octra_api_core::JwtSecret) -> AdminResult<Value> {
    let mut value = to_value(credential)?;
    with_ref(
        &mut value,
        "consumer",
        Some(credential.consumer_id.as_str()),
    );
    Ok(value)
}

fn acl_view(acl: &octra_api_core::Acl) -> AdminResult<Value> {
    let mut value = to_value(acl)?;
    with_ref(&mut value, "consumer", Some(acl.consumer_id.as_str()));
    Ok(value)
}

async fn hash_password(password: String) -> AdminResult<String> {
    tokio::task::spawn_blocking(move || bcrypt::hash(password, bcrypt::DEFAULT_COST))
        .await
        .map_err(|error| AdminError::internal(format!("cannot hash password: {error}")))?
        .map_err(|error| AdminError::schema(format!("cannot hash password: {error}")))
}

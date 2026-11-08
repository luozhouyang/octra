//! Storage tests for API gateway entities.

use std::collections::BTreeMap;

use super::*;
use crate::Storage;

async fn setup() -> Storage {
    Storage::connect_in_memory().await.unwrap()
}

#[tokio::test]
async fn service_crud_and_unique_conflict() {
    let storage = setup().await;
    let service = storage
        .api
        .create_service(&CreateService {
            name: "orders".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(service.host, "orders");
    assert_eq!(service.retries, 5);

    let duplicate = storage
        .api
        .create_service(&CreateService {
            id: Some("svc-2".into()),
            name: "orders".into(),
            ..Default::default()
        })
        .await;
    assert!(matches!(duplicate, Err(Error::Storage(ref msg)) if msg.contains("conflict")));

    let updated = storage
        .api
        .update_service(
            &service.id,
            &UpdateService {
                retries: Some(2),
                description: Some("updated".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.retries, 2);
    assert_eq!(updated.description, "updated");

    storage.api.delete_service(&service.id).await.unwrap();
    assert!(
        storage
            .api
            .get_service(&service.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        storage.api.delete_service(&service.id).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn route_filters_and_parent_checks() {
    let storage = setup().await;
    let service = storage
        .api
        .create_service(&CreateService {
            name: "svc".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    let route = storage
        .api
        .create_route(&CreateRoute {
            name: Some("api".into()),
            service_id: Some(service.id.clone()),
            paths: Some(vec!["/api".into()]),
            methods: Some(vec!["GET".into()]),
            headers: Some(BTreeMap::from([("x-env".into(), vec!["prod".into()])])),
            strip_path: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(route.methods, vec!["GET".to_string()]);
    assert_eq!(route.paths, vec!["/api".to_string()]);
    assert_eq!(route.headers["x-env"], vec!["prod".to_string()]);

    assert_eq!(
        storage
            .api
            .list_routes(Some(&service.id))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        storage
            .api
            .list_routes(Some("missing"))
            .await
            .unwrap()
            .len(),
        0
    );

    let missing = storage
        .api
        .create_route(&CreateRoute {
            service_id: Some("missing".into()),
            ..Default::default()
        })
        .await;
    assert!(matches!(missing, Err(Error::NotFound(_))));

    let cleared = storage
        .api
        .update_route(
            &route.id,
            &UpdateRoute {
                methods: Some(vec![]),
                regex_priority: Some(7),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(cleared.methods.is_empty());
    assert_eq!(cleared.regex_priority, 7);
}

#[tokio::test]
async fn targets_and_runtime_loading() {
    let storage = setup().await;
    let upstream = storage
        .api
        .create_upstream(&CreateUpstream {
            name: "primary".into(),
            algorithm: Some("weighted-round-robin".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(upstream.algorithm, LoadBalanceAlgorithm::WeightedRoundRobin);

    storage
        .api
        .create_target(&CreateTarget {
            upstream_id: upstream.id.clone(),
            target: "127.0.0.1:9000".into(),
            weight: Some(5),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        storage
            .api
            .list_targets(Some(&upstream.id))
            .await
            .unwrap()
            .len(),
        1
    );

    let consumer = storage
        .api
        .create_consumer(&CreateConsumer {
            username: Some("team".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let credential = storage
        .api
        .create_credential(&CreateKeyauthCredential {
            consumer_id: consumer.id.clone(),
            key: Some("ok_abcd1234".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(credential.key, "ok_abcd1234");

    let duplicate = storage
        .api
        .create_credential(&CreateKeyauthCredential {
            consumer_id: consumer.id.clone(),
            key: Some("ok_abcd1234".into()),
            ..Default::default()
        })
        .await;
    assert!(matches!(duplicate, Err(Error::Storage(ref m)) if m.contains("conflict")));

    let data = storage.api.load_runtime_data().await.unwrap();
    assert_eq!(data.upstreams.len(), 1);
    assert_eq!(data.targets.len(), 1);
    assert_eq!(data.consumers.len(), 1);
    assert_eq!(data.keyauth.len(), 1);
}

#[tokio::test]
async fn consumer_groups_and_membership() {
    let storage = setup().await;
    let group = storage
        .api
        .create_consumer_group(&CreateConsumerGroup {
            name: "admins".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let consumer = storage
        .api
        .create_consumer(&CreateConsumer {
            username: Some("alice".into()),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(
        storage
            .api
            .get_consumer_group_by_name("admins")
            .await
            .unwrap()
            .unwrap()
            .id,
        group.id
    );

    storage
        .api
        .add_consumer_group_member(&group.id, &consumer.id)
        .await
        .unwrap();
    let duplicate = storage
        .api
        .add_consumer_group_member(&group.id, &consumer.id)
        .await;
    assert!(matches!(duplicate, Err(Error::Storage(ref m)) if m.contains("conflict")));

    let members = storage
        .api
        .list_consumer_group_members(&group.id)
        .await
        .unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].username.as_deref(), Some("alice"));
    assert_eq!(
        storage
            .api
            .list_consumer_groups_for_consumer(&consumer.id)
            .await
            .unwrap()
            .len(),
        1
    );

    let plugin = storage
        .api
        .create_plugin_instance(&CreatePluginInstance {
            name: "rate-limiting".into(),
            consumer_group_id: Some(group.id.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(plugin.consumer_group_id.as_deref(), Some(group.id.as_str()));

    let data = storage.api.load_runtime_data().await.unwrap();
    assert_eq!(data.consumer_groups.len(), 1);
    assert_eq!(data.consumer_group_members.len(), 1);

    storage
        .api
        .remove_consumer_group_member(&group.id, &consumer.id)
        .await
        .unwrap();
    assert!(
        storage
            .api
            .list_consumer_group_members(&group.id)
            .await
            .unwrap()
            .is_empty()
    );

    let renamed = storage
        .api
        .update_consumer_group(
            &group.id,
            &UpdateConsumerGroup {
                name: Some("operators".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.name, "operators");

    storage.api.delete_consumer_group(&group.id).await.unwrap();
    assert!(
        storage
            .api
            .list_plugin_instances()
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn credentials_are_sealed_at_rest_and_plaintext_to_callers() {
    let storage = setup().await;
    let consumer = storage
        .api
        .create_consumer(&CreateConsumer {
            username: Some("secrets".into()),
            ..Default::default()
        })
        .await
        .unwrap();

    let keyauth = storage
        .api
        .create_credential(&CreateKeyauthCredential {
            consumer_id: consumer.id.clone(),
            key: Some("key-plaintext".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(keyauth.key, "key-plaintext");
    let raw_key: String = sqlx::query_scalar("SELECT key FROM keyauth_credentials WHERE id = ?")
        .bind(&keyauth.id)
        .fetch_one(storage.pool())
        .await
        .unwrap();
    assert!(raw_key.starts_with("enc:v2:"));

    let basicauth = storage
        .api
        .create_basicauth(&CreateBasicauthCredential {
            consumer_id: consumer.id.clone(),
            username: "alice".into(),
            password: "password-plaintext".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(basicauth.password, "password-plaintext");
    let raw_password: String =
        sqlx::query_scalar("SELECT password FROM basicauth_credentials WHERE id = ?")
            .bind(&basicauth.id)
            .fetch_one(storage.pool())
            .await
            .unwrap();
    assert!(raw_password.starts_with("enc:v2:"));

    let jwt = storage
        .api
        .create_jwt(&CreateJwtSecret {
            consumer_id: consumer.id.clone(),
            key: "jwt-key".into(),
            secret: "jwt-secret-plaintext".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(jwt.secret, "jwt-secret-plaintext");
    let raw_jwt: String = sqlx::query_scalar("SELECT secret FROM jwt_secrets WHERE id = ?")
        .bind(&jwt.id)
        .fetch_one(storage.pool())
        .await
        .unwrap();
    assert!(raw_jwt.starts_with("enc:v2:"));
    let raw_jwt_key: String = sqlx::query_scalar("SELECT key FROM jwt_secrets WHERE id = ?")
        .bind(&jwt.id)
        .fetch_one(storage.pool())
        .await
        .unwrap();
    assert_eq!(raw_jwt_key, "jwt-key", "JWT key is not a secret");

    let updated = storage
        .api
        .update_credential(
            &keyauth.id,
            &UpdateKeyauthCredential {
                key: Some("key-rotated".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.key, "key-rotated");
    let raw_key: String = sqlx::query_scalar("SELECT key FROM keyauth_credentials WHERE id = ?")
        .bind(&keyauth.id)
        .fetch_one(storage.pool())
        .await
        .unwrap();
    assert!(raw_key.starts_with("enc:v2:"));
    let lookup: String =
        sqlx::query_scalar("SELECT key_lookup FROM keyauth_credentials WHERE id = ?")
            .bind(&keyauth.id)
            .fetch_one(storage.pool())
            .await
            .unwrap();
    assert_eq!(lookup, crate::key_lookup("key-rotated"));

    let data = storage.api.load_runtime_data().await.unwrap();
    assert_eq!(data.keyauth[0].key, "key-rotated");
    assert_eq!(data.basicauth[0].password, "password-plaintext");
    assert_eq!(data.jwt[0].secret, "jwt-secret-plaintext");

    let wrong = ApiStore::new(
        storage.pool().clone(),
        Arc::new(SecretBox::new(octra_secrets::MasterKey::ephemeral())),
    );
    let error = wrong.get_credential(&keyauth.id).await.unwrap_err();
    let message = error.to_string();
    assert!(message.contains("keyauth_credentials"));
    assert!(message.contains(&keyauth.id));
    assert!(!message.contains("enc:v1:"));
}

#[tokio::test]
async fn keyauth_backfill_populates_the_lookup_and_upgrades_v1() {
    let storage = setup().await;
    let consumer = storage
        .api
        .create_consumer(&CreateConsumer {
            username: Some("lookup".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let legacy = storage.secrets.seal_legacy(b"lookup-key").unwrap();
    sqlx::query(
        "INSERT INTO keyauth_credentials (id, consumer_id, key, tags, created_at) \
         VALUES ('cred_lookup', ?, ?, '[]', CAST(strftime('%s', 'now') AS INTEGER))",
    )
    .bind(&consumer.id)
    .bind(&legacy)
    .execute(storage.pool())
    .await
    .unwrap();

    storage.backfill_secrets().await.unwrap();
    let (key, lookup): (String, Option<String>) =
        sqlx::query_as("SELECT key, key_lookup FROM keyauth_credentials WHERE id = 'cred_lookup'")
            .fetch_one(storage.pool())
            .await
            .unwrap();
    assert!(key.starts_with("enc:v2:"));
    assert_eq!(
        lookup.as_deref(),
        Some(crate::key_lookup("lookup-key").as_str())
    );

    storage.backfill_secrets().await.unwrap();
    let (key_again, lookup_again): (String, Option<String>) =
        sqlx::query_as("SELECT key, key_lookup FROM keyauth_credentials WHERE id = 'cred_lookup'")
            .fetch_one(storage.pool())
            .await
            .unwrap();
    assert_eq!(key, key_again);
    assert_eq!(lookup, lookup_again);
}

#[tokio::test]
async fn concurrent_keyauth_inserts_allow_exactly_one_winner() {
    let storage = setup().await;
    let consumer = storage
        .api
        .create_consumer(&CreateConsumer {
            username: Some("race".into()),
            ..Default::default()
        })
        .await
        .unwrap();

    let input = CreateKeyauthCredential {
        consumer_id: consumer.id.clone(),
        key: Some("shared-race-key".into()),
        ..Default::default()
    };
    let first = storage.api.create_credential(&input);
    let second = storage.api.create_credential(&input);
    let (first, second) = tokio::join!(first, second);

    let successes = [&first, &second]
        .iter()
        .filter(|result| result.is_ok())
        .count();
    assert_eq!(
        successes, 1,
        "exactly one insert must win the unique key race"
    );
    let loser = if first.is_err() { &first } else { &second };
    assert!(
        matches!(loser, Err(Error::Storage(message)) if message.contains("conflict")),
        "{loser:?}"
    );

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM keyauth_credentials")
        .fetch_one(storage.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    let lookup: String = sqlx::query_scalar("SELECT key_lookup FROM keyauth_credentials")
        .fetch_one(storage.pool())
        .await
        .unwrap();
    assert_eq!(lookup, crate::key_lookup("shared-race-key"));
}

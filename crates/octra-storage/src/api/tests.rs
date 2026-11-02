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

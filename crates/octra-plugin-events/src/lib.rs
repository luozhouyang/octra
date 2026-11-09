//! Durable event delivery and scheduled jobs for Octra plugins.
//!
//! The bus persists every published event in SQLite before dispatch, records
//! one delivery row per subscribed plugin instance, and retries failures with
//! exponential backoff. Jobs are reconciled from the currently enabled plugin
//! declarations on every tick, so installing, upgrading or removing a plugin
//! takes effect without a daemon restart.

mod control;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use octra_api_plugin_types::{
    EventPublisher, GatewayEvent, JobInvocation, PluginServiceSource, ScheduledJob, next_run_after,
    next_run_from_now, topic_matches,
};
use octra_core::{Error, Result};
use octra_storage::{EventRecord, NewEvent, NewJob, Storage};
use serde_json::Value;
use tokio::sync::{Notify, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

pub use control::{EventControlState, control_router};

const PUBLISH_QUEUE: usize = 1024;
/// Bound on overflow tasks that wait for queue capacity. Each task holds one
/// permit while it awaits, so a stalled bus cannot create unbounded tasks.
const PUBLISH_OVERFLOW_TASKS: usize = 32;
const DISPATCH_BATCH: u32 = 32;
const DELIVERY_BATCH: u32 = 64;
const JOB_BATCH: u32 = 32;
const DEFAULT_MAX_ATTEMPTS: i64 = 5;
const TICK: Duration = Duration::from_secs(2);

struct PublishMessage {
    topic: String,
    source: String,
    payload_json: String,
}

pub struct EventBus {
    storage: Arc<Storage>,
    source: Arc<dyn PluginServiceSource>,
    sender: mpsc::Sender<PublishMessage>,
    receiver: Mutex<Option<mpsc::Receiver<PublishMessage>>>,
    notify: Arc<Notify>,
    overflow: Arc<Semaphore>,
}

impl EventBus {
    pub fn new(storage: Arc<Storage>, source: Arc<dyn PluginServiceSource>) -> Self {
        let (sender, receiver) = mpsc::channel(PUBLISH_QUEUE);
        Self {
            storage,
            source,
            sender,
            receiver: Mutex::new(Some(receiver)),
            notify: Arc::new(Notify::new()),
            overflow: Arc::new(Semaphore::new(PUBLISH_OVERFLOW_TASKS)),
        }
    }

    pub fn storage(&self) -> &Arc<Storage> {
        &self.storage
    }

    /// Starts the background dispatcher. Calling this more than once is a
    /// no-op; the first call owns the receiver.
    pub fn start(self: &Arc<Self>, shutdown: CancellationToken) {
        let Some(receiver) = self
            .receiver
            .lock()
            .expect("event bus receiver lock poisoned")
            .take()
        else {
            return;
        };
        let bus = self.clone();
        tokio::spawn(async move { bus.run(receiver, shutdown).await });
    }

    /// Persists an event immediately and returns the row. Used by the control
    /// plane and tests; the request path should use [`EventPublisher::publish`].
    pub async fn publish_now(
        &self,
        topic: &str,
        source: &str,
        payload: Value,
    ) -> Result<EventRecord> {
        let payload_json = serde_json::to_string(&payload)
            .map_err(|error| Error::Storage(format!("cannot serialize event: {error}")))?;
        let event = self
            .storage
            .insert_event(&NewEvent {
                topic: topic.to_string(),
                source: source.to_string(),
                payload_json,
                max_attempts: DEFAULT_MAX_ATTEMPTS,
            })
            .await?;
        self.notify.notify_one();
        Ok(event)
    }

    pub async fn trigger_job(&self, id: &str) -> Result<bool> {
        let triggered = self.storage.trigger_job(id, Utc::now().timestamp()).await?;
        if triggered {
            self.notify.notify_one();
        }
        Ok(triggered)
    }

    async fn run(
        self: Arc<Self>,
        mut receiver: mpsc::Receiver<PublishMessage>,
        shutdown: CancellationToken,
    ) {
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                message = receiver.recv() => {
                    match message {
                        Some(message) => self.persist(message).await,
                        None => break,
                    }
                    self.dispatch_events().await;
                    self.sync_and_run_jobs().await;
                }
                _ = ticker.tick() => {
                    self.dispatch_events().await;
                    self.sync_and_run_jobs().await;
                }
                _ = self.notify.notified() => {
                    self.dispatch_events().await;
                    self.sync_and_run_jobs().await;
                }
            }
        }
    }

    async fn persist(&self, message: PublishMessage) {
        match self
            .storage
            .insert_event(&NewEvent {
                topic: message.topic,
                source: message.source,
                payload_json: message.payload_json,
                max_attempts: DEFAULT_MAX_ATTEMPTS,
            })
            .await
        {
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "cannot persist event"),
        }
    }

    async fn dispatch_events(&self) {
        let now = Utc::now().timestamp();
        let events = match self.storage.list_due_events(now, DISPATCH_BATCH).await {
            Ok(events) => events,
            Err(error) => {
                tracing::warn!(%error, "cannot list due events");
                return;
            }
        };

        for event in events {
            if let Err(error) = self.dispatch_one(&event, now).await {
                tracing::warn!(event = %event.id, %error, "event dispatch failed");
            }
        }
    }

    async fn dispatch_one(&self, event: &EventRecord, now: i64) -> Result<()> {
        let subscribers: Vec<_> = self
            .source
            .subscribers()
            .into_iter()
            .filter(|subscriber| {
                subscriber
                    .topics()
                    .iter()
                    .any(|pattern| topic_matches(pattern, &event.topic))
            })
            .collect();

        if subscribers.is_empty() {
            self.storage
                .complete_event_without_deliveries(&event.id, now)
                .await?;
            return Ok(());
        }

        for subscriber in &subscribers {
            self.storage
                .ensure_event_delivery(
                    &event.id,
                    subscriber.subscriber_id(),
                    subscriber.plugin_name(),
                    subscriber.instance_id(),
                    DEFAULT_MAX_ATTEMPTS,
                    now,
                )
                .await?;
        }

        let by_id: HashMap<&str, &Arc<dyn octra_api_plugin_types::EventSubscriber>> = subscribers
            .iter()
            .map(|subscriber| (subscriber.subscriber_id(), subscriber))
            .collect();
        let payload: Value = serde_json::from_str(&event.payload_json).unwrap_or(Value::Null);
        let gateway_event = GatewayEvent {
            id: event.id.clone(),
            topic: event.topic.clone(),
            source: event.source.clone(),
            timestamp: event.created_at,
            payload,
        };

        let deliveries = self
            .storage
            .list_due_event_deliveries(&event.id, now, DELIVERY_BATCH)
            .await?;
        for delivery in deliveries {
            let Some(subscriber) = by_id.get(delivery.subscriber_id.as_str()) else {
                self.storage
                    .mark_event_delivery_failed(
                        &event.id,
                        &delivery.subscriber_id,
                        "subscriber is no longer enabled",
                    )
                    .await?;
                continue;
            };
            match subscriber.deliver(&gateway_event).await {
                Ok(()) => {
                    self.storage
                        .mark_event_delivery_delivered(&event.id, &delivery.subscriber_id, now)
                        .await?;
                }
                Err(error) => {
                    let attempts = delivery.attempts + 1;
                    if attempts >= delivery.max_attempts {
                        self.storage
                            .mark_event_delivery_failed(
                                &event.id,
                                &delivery.subscriber_id,
                                &error.message,
                            )
                            .await?;
                    } else {
                        let next = now + retry_delay(attempts);
                        self.storage
                            .mark_event_delivery_retry(
                                &event.id,
                                &delivery.subscriber_id,
                                next,
                                &error.message,
                            )
                            .await?;
                    }
                }
            }
        }

        self.storage.refresh_event_status(&event.id, now).await?;
        self.storage
            .reschedule_pending_event(&event.id, now)
            .await?;
        Ok(())
    }

    async fn sync_and_run_jobs(&self) {
        let now = Utc::now().timestamp();
        let registrations = self.source.jobs();
        let mut desired = Vec::with_capacity(registrations.len());
        for registration in &registrations {
            let Some(next_run_at) = next_run_from_now(registration.schedule()).ok().flatten()
            else {
                tracing::warn!(
                    job = registration.job_id(),
                    schedule = registration.schedule(),
                    "plugin job has no future cron occurrence"
                );
                continue;
            };
            desired.push(NewJob {
                id: registration.job_id().to_string(),
                plugin_name: registration.plugin_name().to_string(),
                plugin_instance_id: registration.instance_id().to_string(),
                job_name: registration.job_name().to_string(),
                schedule: registration.schedule().to_string(),
                next_run_at,
            });
        }
        if let Err(error) = self.storage.sync_jobs(&desired, now).await {
            tracing::warn!(%error, "cannot sync plugin jobs");
            return;
        }

        let due = match self.storage.list_due_jobs(now, JOB_BATCH).await {
            Ok(due) => due,
            Err(error) => {
                tracing::warn!(%error, "cannot list due plugin jobs");
                return;
            }
        };
        let by_id: HashMap<&str, &Arc<dyn ScheduledJob>> = registrations
            .iter()
            .map(|job| (job.job_id(), job))
            .collect();
        for job in due {
            let Some(registration) = by_id.get(job.id.as_str()) else {
                continue;
            };
            let invocation = JobInvocation {
                id: format!("jobrun_{}", uuid::Uuid::new_v4().simple()),
                name: job.job_name.clone(),
                scheduled_for: job.next_run_at,
                attempt: u32::try_from(job.attempts + 1).unwrap_or(u32::MAX),
            };
            let result =
                tokio::time::timeout(registration.timeout(), registration.run(&invocation)).await;
            let (status, error) = match result {
                Ok(Ok(())) => ("success", None),
                Ok(Err(error)) => ("error", Some(error.message)),
                Err(_) => ("timeout", Some("plugin job timed out".to_string())),
            };
            let next = next_run_after(&job.schedule, now)
                .ok()
                .flatten()
                .unwrap_or(now + 3600);
            if let Err(record_error) = self
                .storage
                .mark_job_result(&job.id, status, error.as_deref(), now, next)
                .await
            {
                tracing::warn!(job = %job.id, %record_error, "cannot record plugin job result");
            }
        }
    }
}

impl EventPublisher for EventBus {
    fn publish(&self, topic: &str, source: &str, payload: Value) {
        // Without a matching subscriber the dispatcher would persist the event
        // and immediately mark it complete, so skip the write entirely. This
        // keeps the outbox empty on deployments with no event plugins.
        let subscribed = self.source.subscribers().iter().any(|subscriber| {
            subscriber
                .topics()
                .iter()
                .any(|pattern| topic_matches(pattern, topic))
        });
        if !subscribed {
            return;
        }
        let payload_json = match serde_json::to_string(&payload) {
            Ok(payload_json) => payload_json,
            Err(error) => {
                tracing::warn!(%error, "cannot serialize event; dropping");
                return;
            }
        };
        let message = PublishMessage {
            topic: topic.to_string(),
            source: source.to_string(),
            payload_json,
        };
        match self.sender.try_send(message) {
            Ok(()) => self.notify.notify_one(),
            Err(mpsc::error::TrySendError::Full(message)) => {
                // Do not drop an event just because the dispatcher is briefly
                // behind. Wait for capacity in a bounded overflow task.
                let Ok(permit) = self.overflow.clone().try_acquire_owned() else {
                    tracing::warn!(
                        topic,
                        "event queue and overflow tasks are full; dropping event"
                    );
                    return;
                };
                let Ok(handle) = tokio::runtime::Handle::try_current() else {
                    tracing::warn!(
                        topic,
                        "event queue is full outside a runtime; dropping event"
                    );
                    return;
                };
                let sender = self.sender.clone();
                let notify = self.notify.clone();
                handle.spawn(async move {
                    let _permit = permit;
                    if sender.send(message).await.is_ok() {
                        notify.notify_one();
                    } else {
                        tracing::warn!("event bus stopped while flushing an overflow event");
                    }
                });
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!(topic, "event bus is stopped; dropping event")
            }
        }
    }
}

fn retry_delay(attempts: i64) -> i64 {
    let exponent = u32::try_from(attempts.clamp(1, 10)).unwrap_or(10);
    (5i64.saturating_mul(1i64 << exponent)).min(3600)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use octra_api_plugin_types::{EventSubscriber, PluginError};
    use serde_json::json;

    use super::*;

    struct FakeSource {
        subscribers: Vec<Arc<dyn EventSubscriber>>,
        jobs: Vec<Arc<dyn ScheduledJob>>,
    }

    impl PluginServiceSource for FakeSource {
        fn subscribers(&self) -> Vec<Arc<dyn EventSubscriber>> {
            self.subscribers.clone()
        }

        fn jobs(&self) -> Vec<Arc<dyn ScheduledJob>> {
            self.jobs.clone()
        }
    }

    struct FakeSubscriber {
        id: String,
        plugin_name: String,
        instance_id: String,
        topics: Vec<String>,
        remaining_failures: AtomicUsize,
        calls: AtomicUsize,
        seen: Mutex<Vec<GatewayEvent>>,
    }

    impl FakeSubscriber {
        fn new(id: &str, topics: &[&str], failures: usize) -> Self {
            Self {
                id: id.to_string(),
                plugin_name: "test-plugin".to_string(),
                instance_id: "test-instance".to_string(),
                topics: topics.iter().map(|topic| (*topic).to_string()).collect(),
                remaining_failures: AtomicUsize::new(failures),
                calls: AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn seen(&self) -> Vec<GatewayEvent> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl EventSubscriber for FakeSubscriber {
        fn subscriber_id(&self) -> &str {
            &self.id
        }

        fn plugin_name(&self) -> &str {
            &self.plugin_name
        }

        fn instance_id(&self) -> &str {
            &self.instance_id
        }

        fn topics(&self) -> &[String] {
            &self.topics
        }

        async fn deliver(&self, event: &GatewayEvent) -> std::result::Result<(), PluginError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(event.clone());
            let failed = self
                .remaining_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok();
            if failed {
                return Err(PluginError::fail_open("subscriber delivery failed"));
            }
            Ok(())
        }
    }

    enum JobBehavior {
        Succeed,
        Fail,
        Sleep(Duration),
    }

    struct FakeJob {
        id: String,
        plugin_name: String,
        instance_id: String,
        job_name: String,
        schedule: String,
        timeout: Duration,
        behavior: JobBehavior,
        runs: AtomicUsize,
    }

    impl FakeJob {
        fn new(id: &str, schedule: &str, timeout: Duration, behavior: JobBehavior) -> Self {
            Self {
                id: id.to_string(),
                plugin_name: "test-plugin".to_string(),
                instance_id: "test-instance".to_string(),
                job_name: "test-job".to_string(),
                schedule: schedule.to_string(),
                timeout,
                behavior,
                runs: AtomicUsize::new(0),
            }
        }

        fn runs(&self) -> usize {
            self.runs.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ScheduledJob for FakeJob {
        fn job_id(&self) -> &str {
            &self.id
        }

        fn plugin_name(&self) -> &str {
            &self.plugin_name
        }

        fn instance_id(&self) -> &str {
            &self.instance_id
        }

        fn job_name(&self) -> &str {
            &self.job_name
        }

        fn schedule(&self) -> &str {
            &self.schedule
        }

        fn timeout(&self) -> Duration {
            self.timeout
        }

        async fn run(&self, _job: &JobInvocation) -> std::result::Result<(), PluginError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            match self.behavior {
                JobBehavior::Succeed => Ok(()),
                JobBehavior::Fail => Err(PluginError::fail_open("boom")),
                JobBehavior::Sleep(duration) => {
                    tokio::time::sleep(duration).await;
                    Ok(())
                }
            }
        }
    }

    async fn in_memory_storage() -> Arc<Storage> {
        Arc::new(Storage::connect_in_memory().await.unwrap())
    }

    fn test_bus(
        storage: &Arc<Storage>,
        subscribers: Vec<Arc<dyn EventSubscriber>>,
        jobs: Vec<Arc<dyn ScheduledJob>>,
    ) -> EventBus {
        EventBus::new(storage.clone(), Arc::new(FakeSource { subscribers, jobs }))
    }

    #[tokio::test]
    async fn delivers_events_to_matching_subscribers() {
        let storage = in_memory_storage().await;
        let subscriber = Arc::new(FakeSubscriber::new("sub-1", &["gateway.*"], 0));
        let bus = test_bus(
            &storage,
            vec![subscriber.clone() as Arc<dyn EventSubscriber>],
            Vec::new(),
        );

        let event = bus
            .publish_now(
                "gateway.request",
                "test",
                serde_json::json!({"request_id": "req-1"}),
            )
            .await
            .unwrap();
        bus.dispatch_events().await;

        let seen = subscriber.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].id, event.id);
        assert_eq!(seen[0].topic, "gateway.request");

        let stored = storage.get_event(&event.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "completed");

        let deliveries = storage.list_event_deliveries(&event.id).await.unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].status, "delivered");
        assert_eq!(deliveries[0].attempts, 1);
    }

    #[tokio::test]
    async fn skips_unsubscribed_topics_at_publish_time() {
        let storage = in_memory_storage().await;
        let subscriber = Arc::new(FakeSubscriber::new("sub-1", &["gateway.*"], 0));
        let bus = test_bus(
            &storage,
            vec![subscriber.clone() as Arc<dyn EventSubscriber>],
            Vec::new(),
        );

        bus.publish("other.topic", "test", serde_json::json!({}));
        assert!(storage.list_events(10, None).await.unwrap().is_empty());

        let event = bus
            .publish_now("other.topic", "test", serde_json::json!({}))
            .await
            .unwrap();
        bus.dispatch_events().await;

        let stored = storage.get_event(&event.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "completed");
        assert!(
            storage
                .list_event_deliveries(&event.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(subscriber.calls(), 0);
    }

    #[tokio::test]
    async fn retries_with_backoff_then_succeeds() {
        let storage = in_memory_storage().await;
        let subscriber = Arc::new(FakeSubscriber::new("sub-1", &["gateway.*"], 1));
        let bus = test_bus(
            &storage,
            vec![subscriber.clone() as Arc<dyn EventSubscriber>],
            Vec::new(),
        );
        let event = bus
            .publish_now("gateway.request", "test", serde_json::json!({}))
            .await
            .unwrap();
        let now = Utc::now().timestamp();

        bus.dispatch_one(&event, now).await.unwrap();

        let pending = storage.get_event(&event.id).await.unwrap().unwrap();
        assert_eq!(pending.status, "pending");
        assert!(pending.next_attempt_at > now);
        let deliveries = storage.list_event_deliveries(&event.id).await.unwrap();
        assert_eq!(deliveries[0].status, "pending");
        assert_eq!(deliveries[0].attempts, 1);
        assert!(deliveries[0].last_error.is_some());
        assert_eq!(subscriber.calls(), 1);

        bus.dispatch_one(&pending, now + 60).await.unwrap();

        let completed = storage.get_event(&event.id).await.unwrap().unwrap();
        assert_eq!(completed.status, "completed");
        let deliveries = storage.list_event_deliveries(&event.id).await.unwrap();
        assert_eq!(deliveries[0].status, "delivered");
        assert_eq!(deliveries[0].attempts, 2);
        assert_eq!(subscriber.calls(), 2);
    }

    #[tokio::test]
    async fn fails_after_max_attempts() {
        let storage = in_memory_storage().await;
        let subscriber = Arc::new(FakeSubscriber::new("sub-1", &["gateway.*"], usize::MAX));
        let bus = test_bus(
            &storage,
            vec![subscriber.clone() as Arc<dyn EventSubscriber>],
            Vec::new(),
        );
        let event = bus
            .publish_now("gateway.request", "test", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(event.max_attempts, 5);

        let mut now = Utc::now().timestamp();
        for attempt in 1..=event.max_attempts {
            let current = storage.get_event(&event.id).await.unwrap().unwrap();
            bus.dispatch_one(&current, now).await.unwrap();

            let deliveries = storage.list_event_deliveries(&event.id).await.unwrap();
            assert_eq!(deliveries[0].attempts, attempt);
            now += 1_000;
        }

        let failed = storage.get_event(&event.id).await.unwrap().unwrap();
        assert_eq!(failed.status, "failed");
        assert!(
            failed
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("subscriber delivery failed"))
        );

        let deliveries = storage.list_event_deliveries(&event.id).await.unwrap();
        assert_eq!(deliveries[0].status, "failed");
        assert_eq!(deliveries[0].attempts, event.max_attempts);
        assert!(deliveries[0].last_error.is_some());
        assert_eq!(subscriber.calls(), event.max_attempts as usize);
    }

    #[tokio::test]
    async fn runs_a_scheduled_job_after_trigger() {
        let storage = in_memory_storage().await;
        let job = Arc::new(FakeJob::new(
            "plugin#instance#job",
            "* * * * * *",
            Duration::from_secs(30),
            JobBehavior::Succeed,
        ));
        let bus = test_bus(
            &storage,
            Vec::new(),
            vec![job.clone() as Arc<dyn ScheduledJob>],
        );

        bus.sync_and_run_jobs().await;
        assert_eq!(job.runs(), 0);

        assert!(bus.trigger_job("plugin#instance#job").await.unwrap());
        bus.sync_and_run_jobs().await;
        assert_eq!(job.runs(), 1);

        let record = storage
            .get_job("plugin#instance#job")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.last_status.as_deref(), Some("success"));
        assert_eq!(record.attempts, 1);
    }

    #[tokio::test]
    async fn records_job_error() {
        let storage = in_memory_storage().await;
        let job = Arc::new(FakeJob::new(
            "plugin#instance#failing",
            "* * * * * *",
            Duration::from_secs(30),
            JobBehavior::Fail,
        ));
        let bus = test_bus(
            &storage,
            Vec::new(),
            vec![job.clone() as Arc<dyn ScheduledJob>],
        );

        bus.sync_and_run_jobs().await;
        assert!(bus.trigger_job("plugin#instance#failing").await.unwrap());
        bus.sync_and_run_jobs().await;

        let record = storage
            .get_job("plugin#instance#failing")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.last_status.as_deref(), Some("error"));
        assert!(
            record
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("boom"))
        );
        assert_eq!(job.runs(), 1);
    }

    #[tokio::test]
    async fn records_job_timeout() {
        let storage = in_memory_storage().await;
        let job = Arc::new(FakeJob::new(
            "plugin#instance#slow",
            "* * * * * *",
            Duration::from_millis(50),
            JobBehavior::Sleep(Duration::from_millis(200)),
        ));
        let bus = test_bus(
            &storage,
            Vec::new(),
            vec![job.clone() as Arc<dyn ScheduledJob>],
        );

        bus.sync_and_run_jobs().await;
        assert!(bus.trigger_job("plugin#instance#slow").await.unwrap());
        bus.sync_and_run_jobs().await;

        let record = storage
            .get_job("plugin#instance#slow")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.last_status.as_deref(), Some("timeout"));
        assert!(
            record
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("timed out"))
        );
        assert_eq!(job.runs(), 1);
    }

    #[test]
    fn retry_delay_is_exponential_and_capped() {
        let mut previous = 0;
        for attempts in 1..=10 {
            let delay = retry_delay(attempts);
            assert!(
                delay >= previous,
                "retry delay decreased at attempt {attempts}"
            );
            assert!(
                delay <= 3600,
                "retry delay exceeded cap at attempt {attempts}"
            );
            previous = delay;
        }
        assert_eq!(retry_delay(1), 10);
        assert_eq!(retry_delay(9), 2560);
        assert_eq!(retry_delay(10), 3600);
        assert_eq!(retry_delay(20), 3600);
    }

    #[tokio::test]
    async fn queue_overflow_is_flushed_instead_of_dropped() {
        let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
        let subscriber = Arc::new(FakeSubscriber::new("sub-overflow", &["test.*"], 0));
        let source = Arc::new(FakeSource {
            subscribers: vec![subscriber.clone()],
            jobs: Vec::new(),
        });
        let bus = Arc::new(EventBus::new(storage, source));
        let total = PUBLISH_QUEUE + 16;
        for index in 0..total {
            bus.publish("test.overflow", "test", json!({"index": index}));
        }
        bus.start(CancellationToken::new());
        let delivered = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if subscriber.calls() >= total {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            delivered.is_ok(),
            "only {} events delivered",
            subscriber.calls()
        );
        assert_eq!(subscriber.calls(), total);
    }
}

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
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

pub use control::{EventControlState, control_router};

const PUBLISH_QUEUE: usize = 1024;
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
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(topic, "event queue is full; dropping event")
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

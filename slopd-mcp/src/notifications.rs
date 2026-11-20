use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rmcp::model::Resource;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;

const PREFIX: &str = "slopd://notifications/";
const LIMIT: usize = 100;

#[derive(Clone, Default)]
pub(crate) struct Notifications(Arc<Mutex<VecDeque<Arc<Watch>>>>);

pub(crate) struct Watch {
    pub(crate) id: String,
    pub(crate) state: watch::Sender<Value>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Some(task) = self
            .task
            .get_mut()
            .unwrap_or_else(|lock| lock.into_inner())
            .take()
        {
            task.abort();
        }
    }
}

impl Notifications {
    pub(crate) fn create(
        &self,
        configuration: Value,
        duration_seconds: u64,
    ) -> Result<Arc<Watch>, &'static str> {
        let mut watches = self.0.lock().unwrap_or_else(|lock| lock.into_inner());
        while watches.len() >= LIMIT {
            let Some(index) = watches
                .iter()
                .position(|watch| watch.state.borrow()["active"] == false)
            else {
                return Err("Too many active notification watches; disable a watch first");
            };
            watches.remove(index);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let watch = Arc::new(Watch {
            state: watch::channel(json!({
                "watch_id": id,
                "resource_uri": format!("{PREFIX}{id}"),
                "active": true,
                "configuration": configuration,
                "expires_at_unix_ms": now_ms() + duration_seconds * 1000,
                "sequence": 0,
                "events": [],
                "subscription_required": true,
                "delivery_instruction": "The MCP host must subscribe to resource_uri and read it after notifications/resources/updated. A notification does not automatically start a model turn. Keep the last sequence to deduplicate events."
            })).0,
            id,
            task: Mutex::new(None),
        });
        watches.push_back(Arc::clone(&watch));
        Ok(watch)
    }

    pub(crate) fn get(&self, id: &str) -> Option<Arc<Watch>> {
        self.0
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .iter()
            .find(|watch| watch.id == id)
            .cloned()
    }

    pub(crate) fn resource(&self, uri: &str) -> Option<Arc<Watch>> {
        self.get(uri.strip_prefix(PREFIX)?)
    }

    pub(crate) fn resources(&self) -> Vec<Resource> {
        self.0
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .iter()
            .map(|watch| watch.resource())
            .collect()
    }
}

impl Watch {
    pub(crate) fn resource(&self) -> Resource {
        Resource::new(format!("{PREFIX}{}", self.id), format!("Agent notifications {}", self.id))
            .with_description("Configured slopd events. Subscribe to this resource, then read updated events and deduplicate by sequence. The latest 100 matching events are retained.")
            .with_mime_type("application/json")
    }

    pub(crate) fn start(&self, task: JoinHandle<()>) {
        *self.task.lock().unwrap_or_else(|lock| lock.into_inner()) = Some(task);
    }

    pub(crate) fn push(&self, event: Value) {
        self.state.send_modify(|state| {
            let sequence = state["sequence"].as_u64().unwrap() + 1;
            state["sequence"] = json!(sequence);
            let events = state["events"].as_array_mut().unwrap();
            if events.len() >= LIMIT {
                events.remove(0);
            }
            let mut event = event;
            event["sequence"] = json!(sequence);
            event["observed_at_unix_ms"] = json!(now_ms());
            events.push(event);
        });
    }

    pub(crate) fn close(&self, reason: &str) {
        self.state.send_modify(|state| {
            state["active"] = json!(false);
            state["closed_reason"] = json!(reason);
        });
    }

    pub(crate) fn stop(&self) {
        if let Some(task) = self
            .task
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .take()
        {
            task.abort();
        }
        self.close("disabled");
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn event_kind(record: &libslop::Record) -> Option<&'static str> {
    match (record.source.as_str(), record.event_type.as_str()) {
        ("hook", "Stop") => Some("completed"),
        ("hook", "StopFailure")
            if record.payload.get("retry_scheduled") == Some(&Value::Bool(false)) =>
        {
            Some("failed")
        }
        ("slopd", "PaneDestroyed") => Some("closed"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduled_and_unclassified_failures_are_not_terminal() {
        let mut record = libslop::Record {
            cursor: None,
            source: "hook".into(),
            event_type: "StopFailure".into(),
            pane_id: Some("%1".into()),
            payload: json!({}),
        };
        assert_eq!(event_kind(&record), None);
        record.payload = json!({"retry_scheduled":true});
        assert_eq!(event_kind(&record), None);
        record.payload = json!({"retry_scheduled":false});
        assert_eq!(event_kind(&record), Some("failed"));
        record.source = "slopd".into();
        record.event_type = "StateChange".into();
        record.payload = json!({"state":"ready"});
        assert_eq!(event_kind(&record), None);
    }

    #[test]
    fn retention_is_bounded_without_reusing_event_sequences() {
        let watches = Notifications::default();
        let watch = watches.create(json!({}), 3600).unwrap();
        for n in 0..150 {
            watch.push(json!({"kind":"completed","n":n}));
        }
        let state = watch.state.borrow();
        assert_eq!(state["events"].as_array().unwrap().len(), 100);
        assert_eq!(state["events"][0]["sequence"], 51);
        assert_eq!(state["sequence"], 150);
        drop(state);
        for _ in 1..LIMIT {
            watches.create(json!({}), 3600).unwrap();
        }
        assert!(watches.create(json!({}), 3600).is_err());
        watch.stop();
        assert!(watches.create(json!({}), 3600).is_ok());
        assert!(watches.get(&watch.id).is_none());
    }
}

//! Ordered, explicitly acknowledged refusal outbox. No automatic eviction.
use super::{Event, PENDING_BYTES};
use serde::{Deserialize, Serialize};
use serde_json::json;
#[cfg(test)]
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Queue {
    pub last_id: u64,
    pub deleted_through: u64,
    pub events: VecDeque<Event>,
    #[serde(skip)]
    pub bytes: usize,
}
impl Queue {
    pub(crate) fn healthy(&self) -> bool {
        self.events.len() < 4096 && self.bytes < PENDING_BYTES && self.last_id < u64::MAX / 2
    }
    pub(crate) fn push(&mut self, mut event: Event) -> u64 {
        self.last_id += 1;
        event.id = self.last_id;
        self.bytes += serde_json::to_vec(&event).unwrap().len();
        self.events.push_back(event);
        self.last_id
    }
    #[cfg(test)]
    pub(crate) fn page(&self, after: u64, limit: usize) -> Value {
        // Binary search over the ordered deque; do not scan other users or old IDs.
        let start = self.events.partition_point(|event| event.id <= after);
        let rows: Vec<_> = self.events.range(start..).take(limit).collect();
        let next = rows
            .last()
            .map_or(after.max(self.deleted_through), |e| e.id);
        json!({"data":rows,"next_after_id":next,"has_more":start + rows.len() < self.events.len(),
            "last_id":self.last_id,"deleted_through":self.deleted_through,"pending":self.events.len()})
    }
    pub(crate) fn delete_through(&mut self, id: u64) -> Option<usize> {
        // Reject future cursors rather than deleting records the caller could not read.
        if id > self.last_id {
            return None;
        }
        let mut deleted = 0;
        while self.events.front().is_some_and(|event| event.id <= id) {
            let event = self.events.pop_front().unwrap();
            self.bytes -= serde_json::to_vec(&event).unwrap().len();
            deleted += 1;
        }
        self.deleted_through = self.deleted_through.max(id);
        Some(deleted)
    }
    fn rebuild(&mut self) -> Result<(), ()> {
        if self.last_id > u64::MAX / 2
            || self.deleted_through > self.last_id
            || self.events.len() > 8192
        {
            return Err(());
        }
        let mut previous = self.deleted_through;
        let mut bytes = 0;
        for event in &self.events {
            if event.id <= previous || event.id > self.last_id || !event.valid() {
                return Err(());
            }
            previous = event.id;
            bytes += serde_json::to_vec(event).map_err(|_| ())?.len();
        }
        self.bytes = bytes;
        Ok(())
    }
}

/// Migrate the old user windows and unacknowledged usage copies once, deduplicated
/// by their old global usage ID. Already evicted history cannot be reconstructed.
pub(crate) fn restore(data: &mut crate::vault::Data) -> Result<(), ()> {
    let has_old = !data.refusals.is_empty()
        || data
            .events
            .iter()
            .any(|v| v["event"]["refusal"].is_object());
    if has_old {
        let queue = &mut data.refusal_queue;
        if queue.last_id != 0 || queue.deleted_through != 0 || !queue.events.is_empty() {
            return Err(());
        }
        let mut imported = BTreeMap::<u64, Event>::new();
        for (user, record) in std::mem::take(&mut data.refusals) {
            if record.recent_events.len() > 20 {
                return Err(());
            }
            for mut event in record.recent_events {
                event.user_id = user.clone();
                if imported.insert(event.id, event).is_some() {
                    return Err(());
                }
            }
        }
        for row in &data.events {
            let audit = &row["event"];
            if audit["refusal"].is_object() {
                let id = row["seq"].as_u64().ok_or(())?;
                let report = serde_json::from_value(audit["refusal"].clone()).map_err(|_| ())?;
                let event = Event::from_audit(id, audit, report);
                if let Some(existing) = imported.get(&id)
                    && serde_json::to_value(existing).map_err(|_| ())?
                        != serde_json::to_value(&event).map_err(|_| ())?
                {
                    return Err(());
                }
                imported.insert(id, event);
            }
        }
        // Preserve old IDs and never reuse IDs that might already be archived.
        if imported.keys().any(|id| *id == 0 || *id > data.sequence) {
            return Err(());
        }
        queue.last_id = data.sequence;
        queue.events = imported.into_values().collect();
    }
    // Refusal bodies have exactly one owner. Usage ACKs and log retention are independent.
    for row in &mut data.events {
        let id = row["seq"].as_u64();
        if let Some(audit) = row["event"].as_object_mut()
            && let Some(old) = audit.remove("refusal")
            && old.is_object()
        {
            audit.insert("refusal_id".into(), json!(id));
        }
    }
    data.refusal_queue.rebuild()
}

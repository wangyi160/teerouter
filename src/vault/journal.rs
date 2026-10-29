//! Sealed, transactional accounting spool. No upstream credentials or prompts.
//! SQLite FULL commits precede upload/ACK responses. Row authentication also
//! covers the complete row set; whole-disk rollback still needs an external head.
use super::*;
use crate::receipt_relay::{Export, Publishing};
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::atomic::Ordering, time::Duration};

const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_ROWS: usize = 210_000;
const MANIFEST: &str = "!manifest-v1";
type Rows = BTreeMap<String, Vec<u8>>;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    instance: String,
    sequence: u64,
    balances: BTreeMap<String, i64>,
    receipt_relay: Option<crate::receipt_relay::State>,
    usage: BTreeMap<String, crate::ledger::Usage>,
    #[serde(default)]
    merchant_refusal_counts: Option<crate::merchants::refusals::Counts>,
    events: VecDeque<Value>,
    pending: Option<Export>,
    last_attempt_ms: Option<u64>,
}
impl Snapshot {
    fn capture(data: &Data, publishing: &Publishing) -> Self {
        Self {
            instance: data.instance.clone(),
            sequence: data.sequence,
            balances: data.balances.clone(),
            receipt_relay: data.receipt_relay.clone(),
            usage: data.usage.clone(),
            merchant_refusal_counts: Some(data.merchant_refusal_counts.clone()),
            events: data.events.clone(),
            pending: publishing.pending.clone(),
            last_attempt_ms: publishing.last_attempt_ms,
        }
    }
    fn rows(&self) -> Result<Rows> {
        let mut value = serde_json::to_value(self).map_err(|_| bad())?;
        let object = value.as_object_mut().ok_or_else(bad)?;
        object.remove("events");
        object.remove("usage");
        // Keep the format marker even when empty, but encrypt each changed
        // customer counter separately instead of rewriting one growing map.
        if self.merchant_refusal_counts.is_some() {
            object.insert("merchant_refusal_counts".into(), json!({}));
        }
        let mut rows = BTreeMap::new();
        for (key, value) in object {
            rows.insert(
                format!("m/{key}"),
                serde_json::to_vec(value).map_err(|_| bad())?,
            );
        }
        for (key, value) in &self.usage {
            rows.insert(
                format!("u/{key}"),
                serde_json::to_vec(value).map_err(|_| bad())?,
            );
        }
        if let Some(counts) = &self.merchant_refusal_counts {
            for (merchant, users) in counts {
                for (user, count) in users {
                    rows.insert(
                        format!("r/{merchant}/{user}"),
                        serde_json::to_vec(count).map_err(|_| bad())?,
                    );
                }
            }
        }
        for row in &self.events {
            let seq = row["seq"].as_u64().ok_or_else(bad)?;
            if rows
                .insert(
                    format!("e/{seq:020}"),
                    serde_json::to_vec(row).map_err(|_| bad())?,
                )
                .is_some()
            {
                return Err(bad());
            }
        }
        if rows.len() > MAX_ROWS || rows.values().map(Vec::len).sum::<usize>() > MAX_BYTES {
            return Err(VaultError("accounting_journal_capacity"));
        }
        Ok(rows)
    }
    fn decode(rows: &Rows) -> Result<Option<Self>> {
        if rows.is_empty() {
            return Ok(None);
        }
        let mut meta = serde_json::Map::new();
        let mut usage = serde_json::Map::new();
        let mut events = vec![];
        let mut counts = serde_json::Map::new();
        for (key, bytes) in rows {
            let value: Value = serde_json::from_slice(bytes).map_err(|_| bad())?;
            if let Some(name) = key.strip_prefix("m/") {
                meta.insert(name.into(), value);
            } else if let Some(name) = key.strip_prefix("u/") {
                usage.insert(name.into(), value);
            } else if let Some(pair) = key.strip_prefix("r/") {
                let (merchant, user) = pair.split_once('/').ok_or_else(bad)?;
                if !crate::config::valid_name(merchant)
                    || !crate::config::valid_name(user)
                    || !value.is_u64()
                {
                    return Err(bad());
                }
                counts
                    .entry(merchant.to_owned())
                    .or_insert_with(|| json!({}))[user] = value;
            } else if let Some(seq) = key.strip_prefix("e/") {
                if value["seq"].as_u64() != seq.parse().ok() {
                    return Err(bad());
                }
                events.push(value);
            } else {
                return Err(bad());
            }
        }
        // Existing relay journals wrote this retired field as null. Accept only
        // that inert marker; never silently discard a private settlement head.
        if meta.remove("receipt_head").is_some_and(|value| !value.is_null()) {
            return Err(VaultError("legacy_private_receipt_journal_requires_migration"));
        }
        meta.insert("usage".into(), Value::Object(usage));
        meta.insert("events".into(), events.into());
        if !counts.is_empty() {
            if meta.get("merchant_refusal_counts") != Some(&json!({})) {
                return Err(bad());
            }
            meta.insert("merchant_refusal_counts".into(), Value::Object(counts));
        }
        let saved: Self = serde_json::from_value(Value::Object(meta)).map_err(|_| bad())?;
        saved.validate()?;
        Ok(Some(saved))
    }
    fn validate(&self) -> Result<()> {
        if crate::receipts::unhex(&self.instance, 16).is_none() || self.sequence > i64::MAX as u64 {
            return Err(bad());
        }
        let before = if let Some(relay) = &self.receipt_relay {
            relay.config.validate()?;
            relay
                .uploaded
                .as_ref()
                .map_or(relay.start_sequence, |h| h.through_seq)
        } else {
            self.events.front().and_then(|r| r["seq"].as_u64())
                .map_or(self.sequence, |s| s.saturating_sub(1))
        };
        let mut seq = before;
        for row in &self.events {
            seq = seq.checked_add(1).ok_or_else(bad)?;
            if row["seq"].as_u64() != Some(seq) {
                return Err(bad());
            }
        }
        if seq != self.sequence {
            return Err(bad());
        }
        Ok(())
    }
}

pub(super) struct Journal {
    db: Connection,
    key: LessSafeKey,
    rows: Rows,
}
fn bad() -> VaultError {
    VaultError("accounting_journal_invalid")
}
fn io(_: impl std::fmt::Display) -> VaultError {
    VaultError("accounting_journal_unavailable")
}
fn digest(rows: &Rows) -> Vec<u8> {
    let mut hash = Sha256::new();
    hash.update(b"teerouter:accounting-journal:v1\0");
    for (name, value) in rows {
        hash.update((name.len() as u64).to_be_bytes());
        hash.update(name.as_bytes());
        hash.update(Sha256::digest(value));
    }
    hash.finalize().to_vec()
}
fn seal(key: &LessSafeKey, name: &str, value: &[u8]) -> Result<Vec<u8>> {
    let mut nonce = [0u8; 12];
    getrandom::getrandom(&mut nonce).map_err(|_| bad())?;
    let mut payload = value.to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(name.as_bytes()),
        &mut payload,
    )
    .map_err(|_| bad())?;
    Ok([nonce.as_slice(), &payload].concat())
}
fn open(key: &LessSafeKey, name: &str, mut value: Vec<u8>) -> Result<Vec<u8>> {
    if value.len() < 28 {
        return Err(bad());
    }
    let nonce: [u8; 12] = value[..12].try_into().map_err(|_| bad())?;
    key.open_in_place(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(name.as_bytes()),
        &mut value[12..],
    )
    .map(|bytes| bytes.to_vec())
    .map_err(|_| bad())
}
impl Journal {
    fn open(dir: &Path, secret: &[u8; 32]) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
        }
        let path = dir.join("accounting.sqlite3");
        let exists = path.exists();
        let db = Connection::open(&path).map_err(io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
        }
        db.busy_timeout(Duration::from_millis(250)).map_err(io)?;
        db.execute_batch(
            "PRAGMA trusted_schema=OFF; PRAGMA locking_mode=EXCLUSIVE;
            PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA wal_autocheckpoint=128;
            PRAGMA journal_size_limit=4194304; PRAGMA max_page_count=65536;
            CREATE TABLE IF NOT EXISTS spool(name TEXT PRIMARY KEY, value BLOB NOT NULL);
            BEGIN EXCLUSIVE; COMMIT;",
        )
        .map_err(io)?;
        let key = LessSafeKey::new(UnboundKey::new(&aead::AES_256_GCM, secret).map_err(|_| bad())?);
        let mut rows = Rows::new();
        let mut root = None;
        let mut bytes = 0usize;
        {
            let mut query = db
                .prepare("SELECT name,value FROM spool ORDER BY name")
                .map_err(io)?;
            let records = query
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .map_err(io)?;
            for row in records {
                let (name, payload) = row.map_err(io)?;
                bytes = bytes.saturating_add(payload.len());
                if bytes > MAX_BYTES + MAX_ROWS * 64 || rows.len() > MAX_ROWS {
                    return Err(bad());
                }
                let plain = open(&key, &name, payload)?;
                if name == MANIFEST {
                    root = Some(plain);
                } else {
                    rows.insert(name, plain);
                }
            }
        }
        if exists || !rows.is_empty() || root.is_some() {
            if root.as_deref() != Some(digest(&rows).as_slice()) {
                return Err(bad());
            }
        } else {
            db.execute(
                "INSERT INTO spool VALUES(?1,?2)",
                params![MANIFEST, seal(&key, MANIFEST, &digest(&rows))?],
            )
            .map_err(io)?;
            // The database name itself must survive a crash, not just its pages.
            #[cfg(unix)]
            std::fs::File::open(dir)
                .and_then(|f| f.sync_all())
                .map_err(io)?;
        }
        Snapshot::decode(&rows)?;
        Ok(Self { db, key, rows })
    }
    fn save(&mut self, data: &Data, publishing: &Publishing) -> Result<()> {
        let snapshot = Snapshot::capture(data, publishing);
        snapshot.validate()?;
        let next = snapshot.rows()?;
        if next == self.rows {
            return Ok(());
        }
        let tx = self.db.transaction().map_err(io)?;
        for (name, value) in &next {
            if self.rows.get(name) != Some(value) {
                tx.execute("INSERT INTO spool VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET value=excluded.value",
                    params![name, seal(&self.key, name, value)?]).map_err(io)?;
            }
        }
        for name in self.rows.keys().filter(|name| !next.contains_key(*name)) {
            tx.execute("DELETE FROM spool WHERE name=?1", [name])
                .map_err(io)?;
        }
        tx.execute(
            "INSERT INTO spool VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET value=excluded.value",
            params![MANIFEST, seal(&self.key, MANIFEST, &digest(&next))?],
        )
        .map_err(io)?;
        tx.commit().map_err(io)?;
        self.rows = next;
        Ok(())
    }
}

impl Vault {
    /// Production persistence keys come only from the measured local dstack guest.
    pub async fn persistent(dir: &Path) -> Result<Arc<Self>> {
        let key = journal_key().await?;
        Self::with_journal(dir, &key)
    }
    pub(crate) fn with_journal(dir: &Path, key: &[u8; 32]) -> Result<Arc<Self>> {
        let journal = Journal::open(dir, key)?;
        let mut vault = Self::memory()?;
        Arc::get_mut(&mut vault).unwrap().journal = Some(Mutex::new(journal));
        Ok(vault)
    }
    pub(crate) fn journal_enabled(&self) -> bool {
        self.journal.is_some()
    }
    pub(crate) fn journal_status(&self) -> Value {
        json!({"enabled":self.journal.is_some(),"healthy":!self.journal_failed.load(Ordering::SeqCst),
            "version":1,"durability":"sqlite_wal_full","max_database_bytes":256*1024*1024})
    }
    pub(crate) fn persist_accounting(&self, data: &Data) -> Result<()> {
        if self.journal.is_none() {
            return Ok(());
        }
        let publishing = self.relay_publishing.lock().unwrap();
        self.persist_accounting_with(data, &publishing)
    }
    pub(crate) fn persist_accounting_with(
        &self,
        data: &Data,
        publishing: &Publishing,
    ) -> Result<()> {
        let Some(journal) = &self.journal else {
            return Ok(());
        };
        if self.journal_failed.load(Ordering::SeqCst) {
            return Err(io("latched"));
        }
        let result = journal.lock().map_err(io)?.save(data, publishing);
        if result.is_err() {
            self.journal_failed.store(true, Ordering::SeqCst);
        }
        result
    }
    pub(super) fn restore_accounting(&self, data: &mut Data) -> Result<()> {
        let Some(journal) = &self.journal else {
            return Ok(());
        };
        let Some(saved) = Snapshot::decode(&journal.lock().map_err(io)?.rows)? else {
            return Ok(());
        };
        if saved.receipt_relay.is_none()
            && saved.events.is_empty()
            && saved
                .merchant_refusal_counts
                .as_ref()
                .is_none_or(|counts| counts.is_empty())
        {
            return Ok(());
        }
        if saved.instance != data.instance || data.sequence > saved.sequence {
            return Err(VaultError("accounting_recovery_conflict"));
        }
        if let Some(local) = &saved.receipt_relay {
            let incoming = data.receipt_relay.as_ref().ok_or_else(bad)?;
            if local.config.url.trim_end_matches('/') != incoming.config.url.trim_end_matches('/')
                || local
                    .config
                    .user_ids
                    .iter()
                    .any(|(id, secret)| incoming.config.user_ids.get(id) != Some(secret))
            {
                return Err(VaultError("accounting_recovery_identity_conflict"));
            }
            let local_seq = local
                .settled
                .as_ref()
                .map_or(local.start_sequence, |h| h.through_seq);
            let manager_seq = incoming
                .settled
                .as_ref()
                .map_or(incoming.start_sequence, |h| h.through_seq);
            if manager_seq < local_seq {
                return Err(VaultError("accounting_recovery_manager_behind"));
            }
            if let Some(head) = &incoming.settled {
                let known = [&local.settled, &local.uploaded]
                    .into_iter()
                    .flatten()
                    .chain(local.retained.iter().map(|r| &r.head))
                    .chain(saved.pending.iter().map(|r| &r.head))
                    .any(|h| {
                        h.batch_hash == head.batch_hash
                            && h.batch_no == head.batch_no
                            && h.through_seq == head.through_seq
                    });
                if !known {
                    return Err(VaultError("accounting_recovery_unknown_head"));
                }
            }
        }
        // Keep the last committed balance and its corresponding spend together.
        // Management applies newer balances via the existing settlement endpoint.
        data.sequence = saved.sequence;
        data.balances = saved.balances;
        data.receipt_relay = saved.receipt_relay;
        data.usage = saved.usage;
        // Pre-upgrade journals have no counter field; retain management's
        // historical backfill in that case. New journals own the exact totals.
        if let Some(counts) = saved.merchant_refusal_counts {
            data.merchant_refusal_counts = counts;
        }
        data.events = saved.events;
        let mut publishing = self.relay_publishing.lock().unwrap();
        publishing.pending = saved.pending;
        publishing.last_attempt_ms = saved.last_attempt_ms;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io)?
            .as_millis() as u64;
        if let Some(at) = saved.last_attempt_ms {
            let age = Duration::from_millis(now_ms.saturating_sub(at).min(3_600_000));
            publishing.last_attempt = Instant::now().checked_sub(age);
        }
        Ok(())
    }
}

async fn journal_key() -> Result<Zeroizing<[u8; 32]>> {
    #[cfg(all(unix, feature = "dstack-tdx"))]
    {
        let client = reqwest::Client::builder()
            .no_proxy()
            .unix_socket("/var/run/dstack.sock")
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(io)?;
        let mut response = client
            .post("http://localhost/GetKey")
            .json(&json!({"path":"teerouter/accounting-journal/v1"}))
            .send()
            .await
            .map_err(io)?
            .error_for_status()
            .map_err(io)?;
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(chunk) = response.chunk().await.map_err(io)? {
            if bytes.len() + chunk.len() > 16_384 {
                return Err(bad());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| bad())?;
        let key: Vec<u8> = if let Some(hex) = value["key"].as_str() {
            crate::receipts::unhex(hex, 32).ok_or_else(bad)?
        } else {
            serde_json::from_value(value["key"].clone()).map_err(|_| bad())?
        };
        let key = Zeroizing::new(key);
        Ok(Zeroizing::new(
            key.as_slice().try_into().map_err(|_| bad())?,
        ))
    }
    #[cfg(not(all(unix, feature = "dstack-tdx")))]
    Err(VaultError(
        "persistent accounting requires local dstack TEE key derivation",
    ))
}

#[cfg(test)]
mod tests;

//! Previewed plans waiting for `execute`. A plan is the exact input that was previewed
//! (a keel-core file `Plan` with its changes and warnings, or a registered operation and
//! its parameters) and the BLAKE3 hash of all of it; `execute` must name both, before the
//! plan expires (`TTL`). The hash is computed again over what is stored, so a plan
//! altered on disk is refused.
//!
//! The store is a file in the library folder (`api-plans.json`, owner-only: `private`),
//! so a `keel plan` and a later `keel execute` work in-process too: only one process holds
//! a library open at a time. Plans of operations with secret parameters (a pairing code)
//! stay in memory only.

use crate::error::{ApiError, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// How long a preview stays executable.
pub const TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Input {
    /// A file operation (`plan`).
    Files(keel_core::Plan),
    /// A registered mutating operation.
    Call { method: String, params: Value },
}

impl Input {
    /// The canonical form that is hashed (everything `execute` runs: file plans with their
    /// changes and warnings): object keys sorted at every level.
    fn canonical(&self) -> Value {
        let v = match self {
            Input::Files(plan) => serde_json::json!({
                "method": "plan",
                "op": plan.op,
                "changes": plan.changes,
                "warnings": plan.warnings,
            }),
            Input::Call { method, params } => {
                serde_json::json!({ "method": method, "params": params })
            }
        };
        sorted(v)
    }

    pub fn hash(&self) -> String {
        let bytes = serde_json::to_vec(&self.canonical()).unwrap_or_default();
        blake3::hash(&bytes).to_hex().to_string()
    }

    pub fn method(&self) -> &str {
        match self {
            Input::Files(_) => "plan",
            Input::Call { method, .. } => method,
        }
    }
}

fn sorted(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        v => v,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Stored {
    input: Input,
    hash: String,
    /// Unix milliseconds.
    created: i64,
    #[serde(default)]
    secret: bool,
}

pub struct PlanStore {
    path: Option<PathBuf>,
    ttl: Duration,
    plans: Mutex<HashMap<String, Stored>>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

impl PlanStore {
    /// Kept in memory only.
    pub fn memory(ttl: Duration) -> Self {
        Self {
            path: None,
            ttl,
            plans: Mutex::new(HashMap::new()),
        }
    }

    /// Backed by `path` (missing or unreadable: starts empty).
    pub fn open(path: PathBuf, ttl: Duration) -> Self {
        let plans = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            path: Some(path),
            ttl,
            plans: Mutex::new(plans),
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    fn expired(&self, s: &Stored, now: i64) -> bool {
        // A clock that went back more than a minute expires plans too.
        now - s.created > self.ttl.as_millis() as i64 || s.created > now + 60_000
    }

    fn save(&self, plans: &HashMap<String, Stored>) {
        let Some(path) = &self.path else { return };
        let kept: HashMap<_, _> = plans.iter().filter(|(_, s)| !s.secret).collect();
        let write =
            || -> std::io::Result<()> { crate::private::write(path, &serde_json::to_vec(&kept)?) };
        if let Err(e) = write() {
            tracing::warn!("saving plans: {e}");
        }
    }

    /// Stores `input`; returns its id, hash and expiry (unix seconds).
    pub fn insert(&self, input: Input, secret: bool) -> Result<(String, String, i64)> {
        let id = random_id()?;
        let hash = input.hash();
        let created = now_ms();
        let mut plans = self.plans.lock();
        plans.retain(|_, s| !self.expired(s, created));
        plans.insert(
            id.clone(),
            Stored {
                input,
                hash: hash.clone(),
                created,
                secret,
            },
        );
        self.save(&plans);
        Ok((id, hash, (created + self.ttl.as_millis() as i64) / 1000))
    }

    /// The operation plan `id` runs, if there is such a plan.
    pub fn method(&self, id: &str) -> Option<String> {
        let plans = self.plans.lock();
        plans.get(id).map(|s| s.input.method().to_owned())
    }

    /// Takes the plan `id` out if `hash` is its input hash and it has not expired. A wrong
    /// hash leaves the plan in place (only the caller holding the preview can run it). The
    /// hash is computed again over the stored input and must match both the stored and the
    /// given hash: a plan altered on disk is dropped.
    pub fn take(&self, id: &str, hash: &str) -> Result<Input> {
        let mut plans = self.plans.lock();
        let now = now_ms();
        let Some(stored) = plans.get(id) else {
            return Err(ApiError::new(
                ApiError::PLAN_EXPIRED,
                format!("no plan {id} (unknown, already executed or expired): preview again"),
            ));
        };
        if self.expired(stored, now) {
            plans.remove(id);
            self.save(&plans);
            return Err(ApiError::new(
                ApiError::PLAN_EXPIRED,
                format!("plan {id} expired: preview again"),
            ));
        }
        let actual = stored.input.hash();
        if !constant_eq(stored.hash.as_bytes(), actual.as_bytes()) {
            plans.remove(id);
            self.save(&plans);
            return Err(ApiError::new(
                ApiError::PLAN_MISMATCH,
                format!("plan {id} was altered after its preview: refused, preview again"),
            ));
        }
        if !constant_eq(actual.as_bytes(), hash.trim().as_bytes()) {
            return Err(ApiError::new(
                ApiError::PLAN_MISMATCH,
                "the input hash does not match the previewed input: refused",
            ));
        }
        let stored = plans.remove(id).expect("checked above");
        self.save(&plans);
        Ok(stored.input)
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 128 random bits, hex.
pub fn random_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| ApiError::failed(format!("random id: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(n: i64) -> Input {
        Input::Call {
            method: "tags.add".into(),
            params: serde_json::json!({ "tag": "x", "paths": [n] }),
        }
    }

    #[test]
    fn hash_is_canonical() {
        let a = Input::Call {
            method: "m".into(),
            params: serde_json::json!({ "b": 1, "a": { "y": 2, "x": 3 } }),
        };
        let b = Input::Call {
            method: "m".into(),
            params: serde_json::from_str(r#"{"a":{"x":3,"y":2},"b":1}"#).unwrap(),
        };
        assert_eq!(a.hash(), b.hash());
        assert_ne!(a.hash(), call(1).hash());
    }

    #[test]
    fn take_checks_hash_and_is_one_shot() {
        let store = PlanStore::memory(TTL);
        let (id, hash, _) = store.insert(call(1), false).unwrap();
        let bad = call(2).hash();
        assert_eq!(
            store.take(&id, &bad).unwrap_err().code,
            ApiError::PLAN_MISMATCH
        );
        assert!(store.take(&id, &hash).is_ok());
        assert_eq!(
            store.take(&id, &hash).unwrap_err().code,
            ApiError::PLAN_EXPIRED
        );
    }

    #[test]
    fn expired_plans_are_refused() {
        let store = PlanStore::memory(Duration::from_millis(30));
        let (id, hash, _) = store.insert(call(1), false).unwrap();
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(
            store.take(&id, &hash).unwrap_err().code,
            ApiError::PLAN_EXPIRED
        );
    }

    /// Stores `input` in a file store at `path`, rewrites `from` as `to` in the file, and
    /// tries to execute the plan with its preview's hash.
    fn tampered(path: &std::path::Path, input: Input, from: &str, to: &str) -> ApiError {
        let (id, hash, _) = PlanStore::open(path.into(), TTL)
            .insert(input, false)
            .unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains(from), "{text}");
        std::fs::write(path, text.replace(from, to)).unwrap();
        let store = PlanStore::open(path.into(), TTL);
        let err = store.take(&id, &hash).unwrap_err();
        assert_eq!(
            store.take(&id, &hash).unwrap_err().code,
            ApiError::PLAN_EXPIRED,
            "an altered plan is dropped"
        );
        err
    }

    #[test]
    fn a_plan_altered_on_disk_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-plans.json");
        let params = Input::Call {
            method: "tags.add".into(),
            params: serde_json::json!({ "tag": "previewed", "paths": [] }),
        };
        let err = tampered(&path, params, "previewed", "other");
        assert_eq!(err.code, ApiError::PLAN_MISMATCH);
        // A file plan: its destination (in `op` and `changes`), or only the changes it runs.
        let at = |name: &str| keel_vfs::VPath::local(dir.path().join(name));
        let plan = keel_core::Plan {
            op: keel_core::Op::Copy {
                src: vec![at("secret.txt")],
                dst_dir: at("previewed_dst"),
                on_conflict: keel_core::OnConflict::Skip,
            },
            changes: vec![keel_core::Change {
                action: keel_core::Action::Copy,
                from: at("secret.txt"),
                to: Some(at("previewed_dst/secret.txt")),
                files: 1,
                bytes: 6,
            }],
            warnings: Vec::new(),
        };
        let err = tampered(
            &path,
            Input::Files(plan.clone()),
            "previewed_dst",
            "other_dst",
        );
        assert_eq!(err.code, ApiError::PLAN_MISMATCH);
        let mut changes_only = plan;
        changes_only.changes[0].to = Some(at("changes_only/secret.txt"));
        let err = tampered(
            &path,
            Input::Files(changes_only),
            "changes_only",
            "other_dst",
        );
        assert_eq!(err.code, ApiError::PLAN_MISMATCH);
        // An untouched file plan survives the round trip through the file.
        let store = PlanStore::open(path.clone(), TTL);
        let (id, hash, _) = store
            .insert(
                Input::Files(keel_core::Plan {
                    op: keel_core::Op::Delete {
                        paths: vec![at("x")],
                    },
                    changes: Vec::new(),
                    warnings: vec![keel_core::Warning::NotIndexed { path: at("x") }],
                }),
                false,
            )
            .unwrap();
        drop(store);
        assert!(PlanStore::open(path, TTL).take(&id, &hash).is_ok());
    }

    #[test]
    fn the_plan_file_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-plans.json");
        PlanStore::open(path.clone(), TTL)
            .insert(call(1), false)
            .unwrap();
        assert!(crate::private::read(&path).unwrap().is_some());
    }

    #[test]
    fn file_store_survives_reopen_but_not_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-plans.json");
        let store = PlanStore::open(path.clone(), TTL);
        let (id, hash, _) = store.insert(call(1), false).unwrap();
        let (sid, shash, _) = store.insert(call(2), true).unwrap();
        drop(store);
        let store = PlanStore::open(path, TTL);
        assert!(store.take(&id, &hash).is_ok());
        assert_eq!(
            store.take(&sid, &shash).unwrap_err().code,
            ApiError::PLAN_EXPIRED
        );
    }
}

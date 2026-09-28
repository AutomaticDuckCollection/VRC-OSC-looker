//! `e` key: one JSON snapshot in the current directory. This is the only
//! place the program writes to disk.
//!
//! Timestamps are seconds since the monitor started; no wall-clock time,
//! machine or user information is included. VRChat IDs are redacted unless
//! `--export-raw` was given. Existing files are never overwritten.

use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};

use crate::state::sanitize::redact_ids;
use crate::state::{EventKind, Store, Value};

const MAX_FILES: u32 = 9999;

/// Drop counters that live outside the store (receiver thread).
pub struct NetCounts {
    pub queue_full: u64,
    pub non_loopback: u64,
}

struct Ctx<'a> {
    store: &'a Store,
    raw: bool,
}

impl Ctx<'_> {
    fn text(&self, s: &str) -> Json {
        Json::String(if self.raw { s.to_string() } else { redact_ids(s) })
    }

    fn secs(&self, t: Instant) -> Json {
        let s = t.saturating_duration_since(self.store.start()).as_secs_f64();
        json!((s * 1000.0).round() / 1000.0)
    }

    fn value(&self, v: &Value) -> Json {
        match v {
            Value::Bool(b) => json!(b),
            Value::Int(i) => json!(i),
            Value::Float(f) => num(*f),
            Value::Str(s) | Value::Other(s) => self.text(s),
        }
    }
}

fn num(f: f64) -> Json {
    if !f.is_finite() {
        return json!(crate::state::fmt_float(f));
    }
    // Most OSC floats are float32: write the shortest float32 form
    // (0.2 rather than 0.19999998807907104).
    let short = f as f32;
    if f64::from(short) == f
        && let Ok(v) = short.to_string().parse::<f64>()
    {
        return json!(v);
    }
    json!(f)
}

fn ms(d: Duration) -> Json {
    json!((d.as_secs_f64() * 1e6).round() / 1e3)
}

/// Builds the snapshot document for `store` as seen at `now`.
pub fn snapshot(store: &Store, now: Instant, raw: bool, net: &NetCounts) -> Json {
    let cx = Ctx { store, raw };
    let epoch = store.epoch();

    let rows: Vec<Json> = store
        .rows()
        .iter()
        .map(|r| {
            let mut row = json!({
                "address": cx.text(&r.addr),
                "type": cx.text(&r.ty),
                "value": cx.value(&r.value),
                "first_seen_s": cx.secs(r.first_seen),
                "last_update_s": cx.secs(r.last_update),
                "value_since_s": cx.secs(r.value_since),
                "messages": r.msgs,
                "changes": r.changes,
                "hz": (r.hz(now) * 10.0).round() / 10.0,
                "previous_avatar": r.is_previous_avatar(epoch),
                "noise": r.noisy,
                "history": r.history.iter().map(|c| json!({
                    "t_s": cx.secs(c.at),
                    "value": cx.value(&c.value),
                    "held_ms": c.held.map(ms),
                })).collect::<Vec<_>>(),
                "distinct": r.distinct.iter().map(|(v, n)| json!({
                    "value": cx.value(v),
                    "count": n,
                })).collect::<Vec<_>>(),
                "distinct_overflow": r.distinct_overflow,
            });
            if let Some(stats) = &r.float {
                let (min, max) = stats.range().map_or((Json::Null, Json::Null), |(a, b)| (num(a), num(b)));
                row["float"] = json!({ "min": min, "max": max, "peak_3s": r.peak(now).map(num) });
            }
            row
        })
        .collect();

    let blips: Vec<Json> = store
        .blips()
        .iter()
        .rev()
        .map(|b| {
            json!({
                "t_s": cx.secs(b.at),
                "address": cx.text(&b.addr),
                "value": cx.value(&b.value),
                "held_ms": ms(b.held),
            })
        })
        .collect();

    let events: Vec<Json> = store
        .events()
        .iter()
        .map(|e| {
            let (kind, detail) = match &e.kind {
                EventKind::AvatarChange { id } => ("avatar_change", cx.text(id)),
                EventKind::NewAddress { ty } => ("new_address", cx.text(ty)),
                EventKind::AddressLimit => ("address_limit", Json::Null),
            };
            json!({
                "t_s": cx.secs(e.at),
                "kind": kind,
                "address": e.addr.as_deref().map(|a| cx.text(a)),
                "detail": detail,
            })
        })
        .collect();

    json!({
        "tool": "vrc-osc-looker",
        "version": env!("CARGO_PKG_VERSION"),
        "snapshot_s": cx.secs(now),
        "ids_redacted": !raw,
        "blip_threshold_ms": ms(store.config().blip_threshold),
        "stats": {
            "packets": store.stats.packets,
            "messages": store.stats.messages,
            "malformed": store.stats.malformed,
            "dropped_queue_full": net.queue_full,
            "dropped_non_loopback": net.non_loopback,
            "ignored_over_address_limit": store.stats.over_cap,
        },
        "rows": rows,
        "blips": blips,
        "events": events,
    })
}

/// Writes `doc` to a new `osc-snapshot-NNN.json` in `dir`, never touching an
/// existing file. Returns the path written.
pub fn write_new(dir: &Path, doc: &Json) -> io::Result<PathBuf> {
    let mut bytes = serde_json::to_vec_pretty(doc).map_err(io::Error::other)?;
    bytes.push(b'\n');
    for n in 1..=MAX_FILES {
        let path = dir.join(format!("osc-snapshot-{n:03}.json"));
        // create_new = O_EXCL / CREATE_NEW: fails if the file exists, atomically.
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                f.write_all(&bytes)?;
                return Ok(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "too many osc-snapshot-*.json files here"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Config, TypeName};

    fn store_with_ids() -> (Store, Instant) {
        let t0 = Instant::now();
        let mut s = Store::new(Config::default(), t0);
        let id = "avtr_c38a1615-5bf5-42b4-84eb-a8b6c37cbd11";
        s.apply(
            "/avatar/change",
            TypeName::Borrowed("string"),
            Value::Str(id.into()),
            t0 + Duration::from_millis(1500),
        );
        s.apply("/x/usr_1234abcd", TypeName::Borrowed("bool"), Value::Bool(true), t0 + Duration::from_secs(2));
        (s, t0 + Duration::from_secs(3))
    }

    #[test]
    fn redacts_ids_and_uses_relative_time() {
        let (s, now) = store_with_ids();
        let net = NetCounts { queue_full: 0, non_loopback: 0 };
        let text = serde_json::to_string(&snapshot(&s, now, false, &net)).unwrap();
        assert!(!text.contains("c38a1615"), "{text}");
        assert!(!text.contains("1234abcd"), "{text}");
        assert!(text.contains("avtr_[redacted]"));
        assert!(text.contains("\"snapshot_s\":3.0"));
        assert!(text.contains("\"t_s\":1.5"));

        let raw = serde_json::to_string(&snapshot(&s, now, true, &net)).unwrap();
        assert!(raw.contains("c38a1615") && raw.contains("1234abcd"));
    }

    #[test]
    fn never_overwrites() {
        let dir = std::env::temp_dir().join(format!("vrc-osc-looker-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("osc-snapshot-001.json");
        std::fs::write(&first, "keep me").unwrap();

        let path = write_new(&dir, &json!({"a": 1})).unwrap();
        assert_eq!(path, dir.join("osc-snapshot-002.json"));
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "keep me");
        let path = write_new(&dir, &json!({"a": 2})).unwrap();
        assert_eq!(path, dir.join("osc-snapshot-003.json"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

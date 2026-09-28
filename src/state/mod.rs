//! Everything the monitor knows about the OSC traffic, as plain data.
//!
//! No I/O happens in here: the receiver thread hands raw packets over, this
//! module decodes, sanitizes, bounds and records them, and the UI reads the
//! result. All text stored here has been through [`sanitize::clean`].

pub mod decode;
pub mod sanitize;
#[cfg(test)]
mod tests;

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

pub const MAX_ADDRESSES: usize = 4096;
pub const MAX_HISTORY: usize = 64;
pub const MAX_DISTINCT: usize = 32;
pub const MAX_BLIPS: usize = 1000;
pub const MAX_EVENTS: usize = 500;

pub const DEFAULT_BLIP_MS: u64 = 500;

pub const AVATAR_CHANGE: &str = "/avatar/change";
const PARAMS_PREFIX: &str = "/avatar/parameters/";

/// Built-in parameters that update almost every frame.
pub const NOISY_PARAMS: [&str; 10] = [
    "VelocityX",
    "VelocityY",
    "VelocityZ",
    "VelocityMagnitude",
    "AngularY",
    "Upright",
    "Voice",
    "Viseme",
    "GestureLeftWeight",
    "GestureRightWeight",
];

/// Human-readable OSC type ("bool", "float", "ifs", ...).
pub type TypeName = Cow<'static, str>;

/// A received value. Strings are already sanitized and bounded.
#[derive(Clone, Debug)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// Pre-rendered text for everything else (blob, color, arrays, multi-arg...).
    Other(String),
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            // Bitwise, so a re-sent NaN is still "the same value".
            (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
            (Value::Str(a), Value::Str(b)) | (Value::Other(a), Value::Other(b)) => a == b,
            _ => false,
        }
    }
}

impl Value {
    /// Blips are tracked for discrete values only; floats use peak-hold.
    pub fn is_blippable(&self) -> bool {
        matches!(self, Value::Bool(_) | Value::Int(_) | Value::Str(_))
    }

    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn display(&self) -> String {
        match self {
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => fmt_float(*f),
            Value::Str(s) => format!("\"{s}\""),
            Value::Other(s) => s.clone(),
        }
    }
}

pub fn fmt_float(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "inf".into() } else { "-inf".into() }
    } else if f != 0.0 && (f.abs() >= 1e6 || f.abs() < 1e-3) {
        format!("{f:.2e}")
    } else {
        format!("{f:.3}")
    }
}

/// Exponentially decaying event rate (time constant 1 s): O(1) memory,
/// accurate for steady streams and falls to zero when an address goes quiet.
#[derive(Clone, Debug, Default)]
pub struct Rate {
    level: f64,
    last: Option<Instant>,
}

impl Rate {
    const TAU_SECS: f64 = 1.0;

    pub fn hit(&mut self, at: Instant) {
        self.level = self.at(at) + 1.0 / Self::TAU_SECS;
        self.last = Some(at);
    }

    pub fn at(&self, now: Instant) -> f64 {
        match self.last {
            Some(last) => {
                let dt = now.saturating_duration_since(last).as_secs_f64();
                self.level * (-dt / Self::TAU_SECS).exp()
            }
            None => 0.0,
        }
    }
}

/// Largest absolute value over the last ~3 s, kept in six 500 ms buckets.
#[derive(Clone, Debug)]
struct PeakHold {
    base: Instant,
    slots: [Option<(u64, f64)>; Self::BUCKETS as usize],
}

impl PeakHold {
    const BUCKET_MS: u128 = 500;
    const BUCKETS: u64 = 6;

    fn new(base: Instant) -> Self {
        Self { base, slots: [None; Self::BUCKETS as usize] }
    }

    fn bucket(&self, t: Instant) -> u64 {
        u64::try_from(t.saturating_duration_since(self.base).as_millis() / Self::BUCKET_MS).unwrap_or(u64::MAX)
    }

    fn record(&mut self, at: Instant, v: f64) {
        if v.is_nan() {
            return;
        }
        let b = self.bucket(at);
        let slot = &mut self.slots[(b % Self::BUCKETS) as usize];
        match slot {
            Some((id, best)) if *id == b => {
                if v.abs() > best.abs() {
                    *best = v;
                }
            }
            _ => *slot = Some((b, v)),
        }
    }

    fn peak(&self, now: Instant) -> Option<f64> {
        let nb = self.bucket(now);
        self.slots
            .iter()
            .flatten()
            .filter(|(id, _)| *id <= nb && id + Self::BUCKETS > nb)
            .map(|&(_, v)| v)
            .max_by(|a, b| a.abs().total_cmp(&b.abs()))
    }
}

#[derive(Clone, Debug)]
pub struct FloatStats {
    min: f64,
    max: f64,
    peak: PeakHold,
}

impl FloatStats {
    fn new(base: Instant) -> Self {
        Self { min: f64::INFINITY, max: f64::NEG_INFINITY, peak: PeakHold::new(base) }
    }

    fn record(&mut self, at: Instant, v: f64) {
        if v.is_nan() {
            return;
        }
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        self.peak.record(at, v);
    }

    /// Session (min, max); `None` if only NaN was ever seen.
    pub fn range(&self) -> Option<(f64, f64)> {
        (self.min <= self.max).then_some((self.min, self.max))
    }
}

/// One entry in an address's change history.
#[derive(Clone, Debug)]
pub struct Change {
    pub at: Instant,
    pub value: Value,
    /// How long the value held; `None` while it is still the current value.
    pub held: Option<Duration>,
}

/// One row of the live table: everything known about one address.
#[derive(Clone, Debug)]
pub struct Row {
    pub addr: Rc<str>,
    pub ty: TypeName,
    pub value: Value,
    pub first_seen: Instant,
    pub last_update: Instant,
    /// When the current value first arrived (re-sends don't move this).
    pub value_since: Instant,
    pub msgs: u64,
    pub changes: u64,
    /// Avatar epoch this address was last seen in.
    pub epoch: u64,
    pub noisy: bool,
    pub float: Option<FloatStats>,
    pub history: VecDeque<Change>,
    pub distinct: Vec<(Value, u64)>,
    /// Messages whose value didn't fit in `distinct`.
    pub distinct_overflow: u64,
    rate: Rate,
}

impl Row {
    fn new(addr: Rc<str>, ty: TypeName, value: Value, at: Instant, epoch: u64, noisy: bool) -> Self {
        let mut row = Row {
            addr,
            ty,
            value: value.clone(),
            first_seen: at,
            last_update: at,
            value_since: at,
            msgs: 1,
            changes: 0,
            epoch,
            noisy,
            float: None,
            history: VecDeque::from([Change { at, value: value.clone(), held: None }]),
            distinct: Vec::new(),
            distinct_overflow: 0,
            rate: Rate::default(),
        };
        row.rate.hit(at);
        row.record_float(at, &value);
        row.count_distinct(&value);
        row
    }

    /// Applies a new message; returns the blip it produced, if any.
    fn update(
        &mut self,
        ty: TypeName,
        value: Value,
        at: Instant,
        epoch: u64,
        blip_threshold: Duration,
    ) -> Option<(Instant, Value, Duration)> {
        self.msgs += 1;
        self.rate.hit(at);
        self.last_update = at;
        self.epoch = epoch;
        if self.ty != ty {
            self.ty = ty;
        }
        self.record_float(at, &value);
        self.count_distinct(&value);
        if value == self.value {
            return None; // identical re-send: not a change
        }

        let held = at.saturating_duration_since(self.value_since);
        if let Some(last) = self.history.back_mut() {
            last.held = Some(held);
        }
        if self.history.len() >= MAX_HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(Change { at, value: value.clone(), held: None });

        let old = std::mem::replace(&mut self.value, value);
        let old_since = std::mem::replace(&mut self.value_since, at);
        self.changes += 1;
        (old.is_blippable() && held < blip_threshold).then_some((old_since, old, held))
    }

    fn record_float(&mut self, at: Instant, value: &Value) {
        if let Some(f) = value.as_float() {
            self.float.get_or_insert_with(|| FloatStats::new(self.first_seen)).record(at, f);
        }
    }

    fn count_distinct(&mut self, value: &Value) {
        if let Some(entry) = self.distinct.iter_mut().find(|(v, _)| v == value) {
            entry.1 += 1;
        } else if self.distinct.len() < MAX_DISTINCT {
            self.distinct.push((value.clone(), 1));
        } else {
            self.distinct_overflow += 1;
        }
    }

    /// Messages per second for this address.
    pub fn hz(&self, now: Instant) -> f64 {
        self.rate.at(now)
    }

    /// Signed value with the largest magnitude over the last ~3 s, including
    /// the value that is currently held. Floats only.
    pub fn peak(&self, now: Instant) -> Option<f64> {
        let stats = self.float.as_ref()?;
        let current = self.value.as_float().filter(|f| !f.is_nan());
        [stats.peak.peak(now), current].into_iter().flatten().max_by(|a, b| a.abs().total_cmp(&b.abs()))
    }

    pub fn is_previous_avatar(&self, current_epoch: u64) -> bool {
        self.epoch < current_epoch
    }
}

/// A discrete value that was replaced within the blip threshold.
#[derive(Clone, Debug)]
pub struct Blip {
    pub seq: u64,
    /// When the short-lived value arrived.
    pub at: Instant,
    pub addr: Rc<str>,
    pub value: Value,
    pub held: Duration,
    pub noisy: bool,
}

#[derive(Clone, Debug)]
pub enum EventKind {
    AvatarChange { id: String },
    NewAddress { ty: TypeName },
    AddressLimit,
}

#[derive(Clone, Debug)]
pub struct Event {
    pub seq: u64,
    pub at: Instant,
    pub addr: Option<Rc<str>>,
    pub noisy: bool,
    pub kind: EventKind,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub blip_threshold: Duration,
    /// Extra address prefixes hidden by the noise filter (`--ignore`).
    pub ignore_prefixes: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self { blip_threshold: Duration::from_millis(DEFAULT_BLIP_MS), ignore_prefixes: Vec::new() }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub packets: u64,
    pub messages: u64,
    pub malformed: u64,
    /// Messages ignored because the address table was full.
    pub over_cap: u64,
}

#[derive(Clone, Debug)]
pub struct Store {
    cfg: Config,
    start: Instant,
    rows: Vec<Row>,
    index: HashMap<Rc<str>, usize>,
    blips: VecDeque<Blip>,
    events: VecDeque<Event>,
    epoch: u64,
    seq: u64,
    limit_logged: bool,
    rate: Rate,
    pub stats: Stats,
}

impl Store {
    pub fn new(cfg: Config, start: Instant) -> Self {
        Self {
            cfg,
            start,
            rows: Vec::new(),
            index: HashMap::new(),
            blips: VecDeque::new(),
            events: VecDeque::new(),
            epoch: 0,
            seq: 0,
            limit_logged: false,
            rate: Rate::default(),
            stats: Stats::default(),
        }
    }

    pub fn start(&self) -> Instant {
        self.start
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn row(&self, addr: &str) -> Option<&Row> {
        self.index.get(addr).map(|&i| &self.rows[i])
    }

    /// Oldest first.
    pub fn blips(&self) -> &VecDeque<Blip> {
        &self.blips
    }

    /// Oldest first.
    pub fn events(&self) -> &VecDeque<Event> {
        &self.events
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Messages per second across all addresses.
    pub fn rate(&self, now: Instant) -> f64 {
        self.rate.at(now)
    }

    pub fn is_noisy(&self, addr: &str) -> bool {
        addr.strip_prefix(PARAMS_PREFIX).is_some_and(|name| NOISY_PARAMS.contains(&name))
            || self.cfg.ignore_prefixes.iter().any(|p| addr.starts_with(p.as_str()))
    }

    /// Decodes one untrusted UDP payload and applies every message in it.
    /// Never panics; bad packets are only counted.
    pub fn ingest_packet(&mut self, bytes: &[u8], at: Instant) {
        self.stats.packets += 1;
        match decode::decode(bytes) {
            Ok(decoded) => {
                if decoded.partial {
                    self.stats.malformed += 1;
                }
                for m in decoded.msgs {
                    self.apply(&m.addr, m.ty, m.value, at);
                }
            }
            Err(_) => self.stats.malformed += 1,
        }
    }

    /// Records one message. `addr` may be raw wire text; it is sanitized here.
    pub fn apply(&mut self, addr: &str, ty: TypeName, value: Value, at: Instant) {
        self.stats.messages += 1;
        self.rate.hit(at);
        let addr = sanitize::clean(addr);

        if addr == AVATAR_CHANGE {
            self.epoch += 1;
            let id = value.display();
            self.push_event(at, Some(Rc::from(addr.as_str())), false, EventKind::AvatarChange { id });
        }

        if let Some(&i) = self.index.get(addr.as_str()) {
            let threshold = self.cfg.blip_threshold;
            if let Some((since, value, held)) = self.rows[i].update(ty, value, at, self.epoch, threshold) {
                self.seq += 1;
                let row = &self.rows[i];
                let blip = Blip { seq: self.seq, at: since, addr: row.addr.clone(), value, held, noisy: row.noisy };
                push_capped(&mut self.blips, blip, MAX_BLIPS);
            }
            return;
        }

        if self.rows.len() >= MAX_ADDRESSES {
            self.stats.over_cap += 1;
            if !self.limit_logged {
                self.limit_logged = true;
                self.push_event(at, None, false, EventKind::AddressLimit);
            }
            return;
        }
        let addr: Rc<str> = Rc::from(addr);
        let noisy = self.is_noisy(&addr);
        self.push_event(at, Some(addr.clone()), noisy, EventKind::NewAddress { ty: ty.clone() });
        self.index.insert(addr.clone(), self.rows.len());
        self.rows.push(Row::new(addr, ty, value, at, self.epoch, noisy));
    }

    /// Forgets all rows, blips and events. Session counters are kept.
    pub fn clear(&mut self) {
        self.rows.clear();
        self.index.clear();
        self.blips.clear();
        self.events.clear();
        self.limit_logged = false;
    }

    fn push_event(&mut self, at: Instant, addr: Option<Rc<str>>, noisy: bool, kind: EventKind) {
        self.seq += 1;
        push_capped(&mut self.events, Event { seq: self.seq, at, addr, noisy, kind }, MAX_EVENTS);
    }
}

fn push_capped<T>(q: &mut VecDeque<T>, item: T, cap: usize) {
    if q.len() >= cap {
        q.pop_front();
    }
    q.push_back(item);
}

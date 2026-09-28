use std::time::{Duration, Instant};

use rosc::{OscArray, OscBundle, OscMessage, OscPacket, OscTime, OscType, encoder};

use super::decode::{self, MAX_ARRAY_DEPTH, MAX_BUNDLE_DEPTH, Malformed};
use super::sanitize::{self, MAX_TEXT_CHARS};
use super::*;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn store() -> (Store, Instant) {
    let t0 = Instant::now();
    (Store::new(Config::default(), t0), t0)
}

fn b(v: bool) -> (TypeName, Value) {
    (TypeName::Borrowed("bool"), Value::Bool(v))
}

fn f(v: f64) -> (TypeName, Value) {
    (TypeName::Borrowed("float"), Value::Float(v))
}

fn send(s: &mut Store, addr: &str, (ty, v): (TypeName, Value), at: Instant) {
    s.apply(addr, ty, v, at);
}

fn msg(addr: &str, args: Vec<OscType>) -> OscPacket {
    OscPacket::Message(OscMessage { addr: addr.into(), args })
}

fn bundle(content: Vec<OscPacket>) -> OscPacket {
    OscPacket::Bundle(OscBundle { timetag: OscTime { seconds: 0, fractional: 1 }, content })
}

/// Hand-built bundle nested `depth` levels deep around one message.
fn nested_bundle(depth: usize) -> Vec<u8> {
    let mut inner = encoder::encode(&msg("/deep", vec![OscType::Int(1)])).unwrap();
    for _ in 0..depth {
        let mut outer = b"#bundle\0\0\0\0\0\0\0\0\x01".to_vec();
        outer.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        outer.extend_from_slice(&inner);
        inner = outer;
    }
    inner
}

/// Deterministic xorshift so the fuzz tests are reproducible.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

// ---- blips -----------------------------------------------------------------

#[test]
fn quick_bool_flick_is_a_blip() {
    let (mut s, t0) = store();
    let a = "/avatar/parameters/Blink";
    send(&mut s, a, b(false), t0);
    send(&mut s, a, b(true), t0 + ms(2000));
    send(&mut s, a, b(false), t0 + ms(2080));

    assert_eq!(s.blips().len(), 1);
    let blip = &s.blips()[0];
    assert_eq!(&*blip.addr, a);
    assert_eq!(blip.value, Value::Bool(true));
    assert_eq!(blip.held, ms(80));
    assert_eq!(blip.at, t0 + ms(2000));
    assert_eq!(s.row(a).unwrap().changes, 2);
}

#[test]
fn slow_change_is_not_a_blip() {
    let (mut s, t0) = store();
    let a = "/avatar/parameters/Toggle";
    send(&mut s, a, b(false), t0);
    send(&mut s, a, b(true), t0 + ms(1000));
    send(&mut s, a, b(false), t0 + ms(1600));
    assert!(s.blips().is_empty());
    assert_eq!(s.row(a).unwrap().changes, 2);
}

#[test]
fn threshold_is_configurable() {
    let t0 = Instant::now();
    let mut s = Store::new(Config { blip_threshold: ms(100), ..Config::default() }, t0);
    send(&mut s, "/x", b(true), t0);
    send(&mut s, "/x", b(false), t0 + ms(150));
    assert!(s.blips().is_empty());
    send(&mut s, "/x", b(true), t0 + ms(200));
    assert_eq!(s.blips().len(), 1);
}

#[test]
fn identical_resends_are_not_changes() {
    let (mut s, t0) = store();
    let a = "/avatar/parameters/Held";
    send(&mut s, a, b(true), t0);
    send(&mut s, a, b(true), t0 + ms(10));
    send(&mut s, a, b(true), t0 + ms(20));
    let r = s.row(a).unwrap();
    assert_eq!((r.msgs, r.changes, r.history.len()), (3, 0, 1));
    assert_eq!(r.value_since, t0);
    assert!(s.blips().is_empty());

    // A re-send doesn't restart the "held" clock: true was held 600 ms, not 300.
    send(&mut s, a, b(false), t0 + ms(1000));
    send(&mut s, a, b(true), t0 + ms(1600));
    send(&mut s, a, b(true), t0 + ms(1900));
    send(&mut s, a, b(false), t0 + ms(2200));
    assert!(s.blips().is_empty(), "{:?}", s.blips());
    assert_eq!(s.row(a).unwrap().history[2].held, Some(ms(600)));
    assert_eq!(s.row(a).unwrap().history.back().unwrap().value, Value::Bool(false));
}

#[test]
fn int_and_string_blips_but_not_floats() {
    let (mut s, t0) = store();
    let int = |v| (TypeName::Borrowed("int"), Value::Int(v));
    send(&mut s, "/i", int(1), t0);
    send(&mut s, "/i", int(2), t0 + ms(1000));
    send(&mut s, "/i", int(3), t0 + ms(1100)); // 2 held 100 ms
    let st = |v: &str| (TypeName::Borrowed("string"), Value::Str(v.into()));
    send(&mut s, "/s", st("a"), t0);
    send(&mut s, "/s", st("b"), t0 + ms(1000));
    send(&mut s, "/s", st("a"), t0 + ms(1010));
    send(&mut s, "/f", f(0.0), t0);
    send(&mut s, "/f", f(1.0), t0 + ms(1000));
    send(&mut s, "/f", f(0.0), t0 + ms(1010));

    let got: Vec<(&str, Value)> = s.blips().iter().map(|b| (&*b.addr, b.value.clone())).collect();
    assert_eq!(got, vec![("/i", Value::Int(2)), ("/s", Value::Str("b".into()))]);
}

#[test]
fn history_records_how_long_each_value_held() {
    let (mut s, t0) = store();
    send(&mut s, "/h", b(false), t0);
    send(&mut s, "/h", b(true), t0 + ms(700));
    send(&mut s, "/h", b(false), t0 + ms(1000));
    let held: Vec<Option<Duration>> = s.row("/h").unwrap().history.iter().map(|c| c.held).collect();
    assert_eq!(held, vec![Some(ms(700)), Some(ms(300)), None]);
    let distinct = &s.row("/h").unwrap().distinct;
    assert_eq!(distinct, &vec![(Value::Bool(false), 2), (Value::Bool(true), 1)]);
}

// ---- floats ----------------------------------------------------------------

#[test]
fn float_min_max_and_peak_hold() {
    let (mut s, t0) = store();
    let a = "/avatar/parameters/VelocityY";
    send(&mut s, a, f(0.1), t0);
    send(&mut s, a, f(-2.5), t0 + ms(100)); // spike
    send(&mut s, a, f(0.3), t0 + ms(200));
    send(&mut s, a, f(1.0), t0 + ms(300));
    send(&mut s, a, f(f64::NAN), t0 + ms(400)); // ignored by min/max/peak

    let r = s.row(a).unwrap();
    assert_eq!(r.float.as_ref().unwrap().range(), Some((-2.5, 1.0)));
    assert_eq!(r.peak(t0 + ms(500)), Some(-2.5));
    assert_eq!(r.peak(t0 + ms(2900)), Some(-2.5), "spike still held within ~3 s");

    send(&mut s, a, f(0.2), t0 + ms(3600));
    let r = s.row(a).unwrap();
    assert_eq!(r.peak(t0 + ms(3600)), Some(0.2), "spike expired");
    assert_eq!(r.float.as_ref().unwrap().range(), Some((-2.5, 1.0)), "session min/max persist");
    // The held value counts toward the peak even when nothing new arrives.
    assert_eq!(r.peak(t0 + ms(60_000)), Some(0.2));
}

#[test]
fn rate_tracks_steady_stream_and_decays() {
    let (mut s, t0) = store();
    for i in 0..300u64 {
        send(&mut s, "/v", f(i as f64), t0 + Duration::from_micros(i * 16_667));
    }
    let r = s.row("/v").unwrap();
    let last = r.last_update;
    let hz = r.hz(last);
    assert!((58.0..=62.0).contains(&hz), "{hz}");
    assert!(r.hz(last + Duration::from_secs(10)) < 0.01);
}

// ---- avatar switches & noise -------------------------------------------------

#[test]
fn avatar_change_marks_previous_rows() {
    let (mut s, t0) = store();
    let avatar = |id: &str| (TypeName::Borrowed("string"), Value::Str(id.into()));
    send(&mut s, AVATAR_CHANGE, avatar("avtr_a"), t0);
    send(&mut s, "/avatar/parameters/OnlyOnA", b(true), t0 + ms(10));
    send(&mut s, "/avatar/parameters/Shared", b(true), t0 + ms(10));
    send(&mut s, AVATAR_CHANGE, avatar("avtr_b"), t0 + ms(5000));
    send(&mut s, "/avatar/parameters/Shared", b(false), t0 + ms(5010));

    let e = s.epoch();
    assert!(s.row("/avatar/parameters/OnlyOnA").unwrap().is_previous_avatar(e));
    assert!(!s.row("/avatar/parameters/Shared").unwrap().is_previous_avatar(e));
    assert!(!s.row(AVATAR_CHANGE).unwrap().is_previous_avatar(e));
    assert_eq!(s.rows().len(), 3, "nothing is deleted");
    let markers: Vec<&str> = s
        .events()
        .iter()
        .filter_map(|ev| match &ev.kind {
            EventKind::AvatarChange { id } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(markers, vec!["\"avtr_a\"", "\"avtr_b\""]);
}

#[test]
fn noise_classification() {
    let t0 = Instant::now();
    let s = Store::new(Config { ignore_prefixes: vec!["/tracking/".into()], ..Config::default() }, t0);
    for n in NOISY_PARAMS {
        assert!(s.is_noisy(&format!("/avatar/parameters/{n}")));
    }
    assert!(s.is_noisy("/tracking/eye/x"));
    assert!(!s.is_noisy("/avatar/parameters/VelocityXtra"));
    assert!(!s.is_noisy("/avatar/parameters/Blink"));
}

#[test]
fn clear_forgets_everything_but_counters() {
    let (mut s, t0) = store();
    send(&mut s, "/a", b(true), t0);
    send(&mut s, "/a", b(false), t0 + ms(10));
    s.clear();
    assert!(s.rows().is_empty() && s.blips().is_empty() && s.events().is_empty());
    assert!(s.row("/a").is_none());
    assert_eq!(s.stats.messages, 2);
    send(&mut s, "/a", b(true), t0 + ms(20));
    assert_eq!(s.row("/a").unwrap().msgs, 1);
}

// ---- caps --------------------------------------------------------------------

#[test]
fn address_cap_holds() {
    let (mut s, t0) = store();
    for i in 0..MAX_ADDRESSES + 100 {
        send(&mut s, &format!("/a/{i}"), b(true), t0);
    }
    assert_eq!(s.rows().len(), MAX_ADDRESSES);
    assert_eq!(s.stats.over_cap, 100);
    let limits = s.events().iter().filter(|e| matches!(e.kind, EventKind::AddressLimit)).count();
    assert_eq!(limits, 1);
    // Existing rows still update.
    send(&mut s, "/a/0", b(false), t0 + ms(1));
    assert_eq!(s.row("/a/0").unwrap().changes, 1);
}

#[test]
fn history_distinct_blip_and_event_caps_hold() {
    let (mut s, t0) = store();
    for i in 0..(MAX_BLIPS as u64 * 2) {
        send(&mut s, "/i", (TypeName::Borrowed("int"), Value::Int(i as i64)), t0 + ms(i));
    }
    let r = s.row("/i").unwrap();
    assert_eq!(r.history.len(), MAX_HISTORY);
    assert_eq!(r.history.back().unwrap().value, Value::Int(MAX_BLIPS as i64 * 2 - 1));
    assert_eq!(r.distinct.len(), MAX_DISTINCT);
    assert_eq!(r.distinct_overflow, MAX_BLIPS as u64 * 2 - MAX_DISTINCT as u64);
    assert_eq!(s.blips().len(), MAX_BLIPS);

    for i in 0..MAX_EVENTS * 2 {
        send(&mut s, &format!("/e/{i}"), b(true), t0);
    }
    assert_eq!(s.events().len(), MAX_EVENTS);
}

#[test]
fn text_and_blob_caps_hold() {
    let (mut s, t0) = store();
    let long_addr = format!("/{}", "a".repeat(10_000));
    let long_str = "é".repeat(10_000);
    let pkt = encoder::encode(&msg(&long_addr, vec![OscType::String(long_str)])).unwrap();
    s.ingest_packet(&pkt, t0);
    let r = &s.rows()[0];
    assert!(r.addr.chars().count() <= MAX_TEXT_CHARS);
    assert!(r.addr.ends_with('…'));
    let Value::Str(v) = &r.value else { panic!("{:?}", r.value) };
    assert!(v.chars().count() <= MAX_TEXT_CHARS);

    let pkt = encoder::encode(&msg("/blob", vec![OscType::Blob(vec![0xAB; 30_000])])).unwrap();
    s.ingest_packet(&pkt, t0);
    let Value::Other(text) = &s.row("/blob").unwrap().value else { panic!() };
    assert!(text.starts_with("blob[30000] abab"), "{text}");
    assert!(text.len() < 64, "{text}");
}

// ---- sanitizer -------------------------------------------------------------

#[test]
fn sanitizer_escapes_control_chars() {
    assert_eq!(sanitize::clean("a\x1b[31mb"), "a\\x1b[31mb");
    assert_eq!(sanitize::clean("\u{9b}2J"), "\\x9b2J"); // 8-bit CSI
    assert_eq!(sanitize::clean("x\ny\tz\r\x07\x00\x7f"), "x\\ny\\tz\\r\\x07\\x00\\x7f");
    assert_eq!(sanitize::clean("ab\u{202e}cd\u{2028}"), "ab\\u{202e}cd\\u{2028}");
    assert_eq!(sanitize::clean("/avatar/parameters/Émoji_✓"), "/avatar/parameters/Émoji_✓");

    let nasty: String = (0u32..0x3000).filter_map(char::from_u32).collect();
    let out = sanitize::clean_max(&nasty, 100_000);
    assert!(!out.chars().any(|c| c.is_control()), "{out:?}");
}

#[test]
fn sanitizer_truncates_after_escaping() {
    let out = sanitize::clean(&"\x1b".repeat(1000));
    assert!(out.chars().count() <= MAX_TEXT_CHARS);
    assert!(out.ends_with('…'));
    assert!(!out.contains('\x1b'));
    // Exactly at the limit: untouched.
    let exact = "x".repeat(MAX_TEXT_CHARS);
    assert_eq!(sanitize::clean(&exact), exact);
    assert_eq!(sanitize::clean_max("abcdef", 4), "abc…");
}

#[test]
fn wire_text_is_sanitized_before_storage() {
    let (mut s, t0) = store();
    let pkt = encoder::encode(&msg("/evil\x1b]0;pwned\x07", vec![OscType::String("\x1b[2J\x1b[H".into())])).unwrap();
    s.ingest_packet(&pkt, t0);
    let r = &s.rows()[0];
    assert_eq!(&*r.addr, "/evil\\x1b]0;pwned\\x07");
    assert_eq!(r.value, Value::Str("\\x1b[2J\\x1b[H".into()));
    let ev = s.events().back().unwrap();
    assert!(!ev.addr.as_deref().unwrap().contains('\x1b'));
}

#[test]
fn redacts_vrchat_ids() {
    let r = sanitize::redact_ids;
    assert_eq!(r("avtr_c38a1615-5bf5-42b4-84eb-a8b6c37cbd11"), "avtr_[redacted]");
    assert_eq!(
        r("\"usr_0123abcd-0000-1111-2222-333344445555\" in wrld_ab12-cd:12345~grp_99"),
        "\"usr_[redacted]\" in wrld_[redacted]:12345~grp_[redacted]"
    );
    assert_eq!(r("AVTR_ABC"), "AVTR_[redacted]");
    assert_eq!(r("/avatar/parameters/Blink avtr_"), "/avatar/parameters/Blink avtr_");
    assert_eq!(r("héllo wörld_"), "héllo wörld_");
}

// ---- untrusted input ---------------------------------------------------------

#[test]
fn decodes_vrchat_style_messages_and_bundles() {
    let (mut s, t0) = store();
    let pkt = bundle(vec![
        msg("/avatar/parameters/A", vec![OscType::Bool(true)]),
        msg("/avatar/parameters/B", vec![OscType::Int(7)]),
        bundle(vec![msg("/avatar/parameters/C", vec![OscType::Float(0.5)])]),
        msg("/multi", vec![OscType::Int(1), OscType::String("x".into()), OscType::Nil]),
        msg("/none", vec![]),
    ]);
    s.ingest_packet(&encoder::encode(&pkt).unwrap(), t0);
    assert_eq!(s.stats.malformed, 0);
    assert_eq!(s.row("/avatar/parameters/A").unwrap().value, Value::Bool(true));
    assert_eq!(s.row("/avatar/parameters/B").unwrap().value, Value::Int(7));
    assert_eq!(s.row("/avatar/parameters/C").unwrap().value, Value::Float(0.5));
    let multi = s.row("/multi").unwrap();
    assert_eq!((&*multi.ty, multi.value.display().as_str()), ("isN", "1, \"x\", nil"));
    assert_eq!(&*s.row("/none").unwrap().ty, "none");
}

#[test]
fn random_bytes_never_panic() {
    let (mut s, t0) = store();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..20_000 {
        let len = rng.below(600);
        let mut buf: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        // Bias some inputs toward looking like OSC so the parser gets deeper.
        match rng.below(4) {
            0 if len >= 8 => buf[..8].copy_from_slice(b"#bundle\0"),
            1 if len >= 1 => buf[0] = b'/',
            _ => {}
        }
        assert_ne!(decode::decode(&buf).err(), Some(Malformed::Panicked));
        s.ingest_packet(&buf, t0);
    }
    assert!(s.stats.malformed > 0);
}

#[test]
fn truncated_and_mutated_packets_never_panic() {
    let (mut s, t0) = store();
    let pkt = encoder::encode(&bundle(vec![
        msg("/a", vec![OscType::Bool(true), OscType::String("hello".into())]),
        msg("/b", vec![OscType::Blob(vec![1, 2, 3, 4, 5]), OscType::Double(1.5), OscType::Long(-3)]),
        bundle(vec![msg("/c", vec![OscType::Array(OscArray { content: vec![OscType::Int(1), OscType::Char('x')] })])]),
    ]))
    .unwrap();
    for cut in 0..=pkt.len() {
        assert_ne!(decode::decode(&pkt[..cut]).err(), Some(Malformed::Panicked));
        s.ingest_packet(&pkt[..cut], t0);
    }
    let mut rng = Rng(42);
    for _ in 0..20_000 {
        let mut m = pkt.clone();
        for _ in 0..1 + rng.below(4) {
            let i = rng.below(m.len());
            m[i] = rng.next() as u8;
        }
        assert_ne!(decode::decode(&m).err(), Some(Malformed::Panicked));
        s.ingest_packet(&m, t0);
    }
    assert!(s.rows().len() <= MAX_ADDRESSES);
}

#[test]
fn nesting_limits_hold_without_deep_recursion() {
    // Run on a deliberately small stack: if anything recursed per nesting
    // level, 10k levels would overflow it and abort the test binary.
    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            assert!(decode::decode(&nested_bundle(MAX_BUNDLE_DEPTH)).is_ok());
            assert_eq!(decode::decode(&nested_bundle(MAX_BUNDLE_DEPTH + 1)).err(), Some(Malformed::TooDeep));
            assert_eq!(decode::decode(&nested_bundle(10_000)).err(), Some(Malformed::TooDeep));

            let arrays = |depth: usize| {
                let mut tags = String::from(",");
                tags.push_str(&"[".repeat(depth));
                tags.push('i');
                tags.push_str(&"]".repeat(depth));
                let mut p = b"/arr\0\0\0\0".to_vec();
                p.extend_from_slice(&encoder::encode_string(tags));
                p.extend_from_slice(&1i32.to_be_bytes());
                p
            };
            assert!(decode::decode(&arrays(MAX_ARRAY_DEPTH)).is_ok());
            assert_eq!(decode::decode(&arrays(MAX_ARRAY_DEPTH + 1)).err(), Some(Malformed::TooDeep));
            assert_eq!(decode::decode(&arrays(20_000)).err(), Some(Malformed::TooDeep));
        })
        .unwrap();
    handle.join().unwrap();

    let (mut s, t0) = store();
    s.ingest_packet(&nested_bundle(MAX_BUNDLE_DEPTH), t0);
    assert_eq!(s.row("/deep").unwrap().value, Value::Int(1));
    s.ingest_packet(&nested_bundle(50_000), t0);
    assert_eq!(s.stats.malformed, 1);
}

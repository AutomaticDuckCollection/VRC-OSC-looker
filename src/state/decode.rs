//! Untrusted UDP payload -> flat list of messages.
//!
//! Every packet goes through a cheap structural pre-check before `rosc` sees
//! it. The pre-check limits bundle and array nesting, so neither `rosc`'s
//! recursive bundle parser nor the recursive drop of nested arrays can blow
//! the stack. The `rosc` call is also wrapped in `catch_unwind` as a last line
//! of defence: a parser bug becomes a counted malformed packet, not a crash.

use std::cell::Cell;
use std::fmt::Write as _;

use rosc::{OscMessage, OscPacket, OscType};

use super::sanitize;
use super::{TypeName, Value};

/// Maximum nesting of bundles (a top-level bundle is depth 1).
pub const MAX_BUNDLE_DEPTH: usize = 8;
/// Maximum nesting of OSC arrays (`[` ... `]`) inside one message.
pub const MAX_ARRAY_DEPTH: usize = 8;

/// Upper bound on rendered text before it is sanitized and truncated.
const RENDER_BUDGET: usize = 2 * sanitize::MAX_TEXT_CHARS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Malformed {
    /// Not an OSC message or bundle, or inconsistent bundle framing.
    Structure,
    /// Bundles or arrays nested deeper than allowed.
    TooDeep,
    /// `rosc` rejected it.
    Osc,
    /// The parser panicked (contained, never propagated).
    Panicked,
}

#[derive(Debug)]
pub struct Msg {
    /// Raw address as sent (sanitized by the store).
    pub addr: String,
    pub ty: TypeName,
    pub value: Value,
}

#[derive(Debug)]
pub struct Decoded {
    pub msgs: Vec<Msg>,
    /// Some of the packet could not be decoded (trailing or invalid bytes).
    pub partial: bool,
}

thread_local! {
    static CONTAINED: Cell<bool> = const { Cell::new(false) };
}

/// True while this thread is inside the contained parser call. The panic
/// hook uses this to stay quiet (and leave the terminal alone) for panics
/// that `decode` will catch and count.
pub fn panic_is_contained() -> bool {
    CONTAINED.with(Cell::get)
}

pub fn decode(bytes: &[u8]) -> Result<Decoded, Malformed> {
    precheck(bytes, 1)?;

    CONTAINED.with(|c| c.set(true));
    let result = std::panic::catch_unwind(|| rosc::decoder::decode_udp(bytes));
    CONTAINED.with(|c| c.set(false));

    let (rest, packet) = result.map_err(|_| Malformed::Panicked)?.map_err(|_| Malformed::Osc)?;
    let mut msgs = Vec::new();
    flatten(packet, 1, &mut msgs)?;
    Ok(Decoded { msgs, partial: !rest.is_empty() })
}

/// Walks the packet framing without allocating, rejecting anything nested
/// too deeply. Recursion depth is bounded by `MAX_BUNDLE_DEPTH`.
fn precheck(p: &[u8], depth: usize) -> Result<(), Malformed> {
    if p.starts_with(b"#bundle\0") {
        if depth > MAX_BUNDLE_DEPTH {
            return Err(Malformed::TooDeep);
        }
        // "#bundle\0" + 8-byte time tag, then (u32 size, element) pairs.
        let mut rest = p.get(16..).ok_or(Malformed::Structure)?;
        while let Some((size, tail)) = rest.split_first_chunk::<4>() {
            let size = u32::from_be_bytes(*size) as usize;
            // Element sizes must keep 4-byte alignment (OSC 1.0), which also
            // keeps this walk in step with how rosc pads strings.
            if !size.is_multiple_of(4) {
                return Err(Malformed::Structure);
            }
            let elem = tail.get(..size).ok_or(Malformed::Structure)?;
            precheck(elem, depth + 1)?;
            rest = &tail[size..];
        }
        Ok(())
    } else if p.first() == Some(&b'/') {
        check_array_depth(p)
    } else {
        Err(Malformed::Structure)
    }
}

/// Finds the type tag string exactly where rosc will read it and bounds
/// its `[` nesting.
fn check_array_depth(msg: &[u8]) -> Result<(), Malformed> {
    let Some(nul) = msg.iter().position(|&b| b == 0) else {
        return Err(Malformed::Structure);
    };
    let tags = msg.get((nul + 1).next_multiple_of(4)..).unwrap_or_default();
    let mut depth = 0usize;
    for &b in tags.iter().take_while(|&&b| b != 0) {
        match b {
            b'[' => {
                depth += 1;
                if depth > MAX_ARRAY_DEPTH {
                    return Err(Malformed::TooDeep);
                }
            }
            b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

fn flatten(packet: OscPacket, depth: usize, out: &mut Vec<Msg>) -> Result<(), Malformed> {
    match packet {
        OscPacket::Message(m) => out.push(convert(m)),
        OscPacket::Bundle(b) => {
            if depth > MAX_BUNDLE_DEPTH {
                return Err(Malformed::TooDeep);
            }
            for p in b.content {
                flatten(p, depth + 1, out)?;
            }
        }
    }
    Ok(())
}

fn convert(m: OscMessage) -> Msg {
    let (ty, value) = match m.args.as_slice() {
        [] => (TypeName::Borrowed("none"), Value::Other("(no args)".into())),
        [arg] => single(arg),
        args => {
            let mut tags = String::new();
            let mut text = String::new();
            for (i, a) in args.iter().enumerate() {
                if tags.len() < 16 {
                    push_tag(&mut tags, a, 0);
                }
                if text.len() < RENDER_BUDGET {
                    if i > 0 {
                        text.push_str(", ");
                    }
                    render(&mut text, a, 0);
                }
            }
            let tags = sanitize::clean_max(&tags, 12);
            (TypeName::Owned(tags), Value::Other(sanitize::clean(&text)))
        }
    };
    Msg { addr: m.addr, ty, value }
}

fn single(arg: &OscType) -> (TypeName, Value) {
    let (ty, value) = match arg {
        OscType::Bool(b) => ("bool", Value::Bool(*b)),
        OscType::Int(i) => ("int", Value::Int(i64::from(*i))),
        OscType::Long(l) => ("long", Value::Int(*l)),
        OscType::Float(f) => ("float", Value::Float(f64::from(*f))),
        OscType::Double(d) => ("double", Value::Float(*d)),
        OscType::String(s) => ("string", Value::Str(sanitize::clean(s))),
        OscType::Char(c) => ("char", Value::Str(sanitize::clean(c.encode_utf8(&mut [0; 4])))),
        other => {
            let mut text = String::new();
            render(&mut text, other, 0);
            (tag_name(other), Value::Other(sanitize::clean(&text)))
        }
    };
    (TypeName::Borrowed(ty), value)
}

fn tag_name(arg: &OscType) -> &'static str {
    match arg {
        OscType::Int(_) => "int",
        OscType::Float(_) => "float",
        OscType::String(_) => "string",
        OscType::Blob(_) => "blob",
        OscType::Time(_) => "time",
        OscType::Long(_) => "long",
        OscType::Double(_) => "double",
        OscType::Char(_) => "char",
        OscType::Color(_) => "color",
        OscType::Midi(_) => "midi",
        OscType::Bool(_) => "bool",
        OscType::Array(_) => "array",
        OscType::Nil => "nil",
        OscType::Inf => "inf",
    }
}

fn push_tag(out: &mut String, arg: &OscType, depth: usize) {
    let c = match arg {
        OscType::Int(_) => 'i',
        OscType::Float(_) => 'f',
        OscType::String(_) => 's',
        OscType::Blob(_) => 'b',
        OscType::Time(_) => 't',
        OscType::Long(_) => 'h',
        OscType::Double(_) => 'd',
        OscType::Char(_) => 'c',
        OscType::Color(_) => 'r',
        OscType::Midi(_) => 'm',
        OscType::Bool(true) => 'T',
        OscType::Bool(false) => 'F',
        OscType::Nil => 'N',
        OscType::Inf => 'I',
        OscType::Array(a) => {
            out.push('[');
            if depth < MAX_ARRAY_DEPTH {
                for x in a.content.iter().take(16) {
                    push_tag(out, x, depth + 1);
                }
            }
            ']'
        }
    };
    out.push(c);
}

/// Renders one argument as display text. Output is only roughly bounded
/// here; the caller sanitizes and truncates it.
fn render(out: &mut String, arg: &OscType, depth: usize) {
    if out.len() >= RENDER_BUDGET {
        return;
    }
    match arg {
        OscType::Int(i) => {
            let _ = write!(out, "{i}");
        }
        OscType::Long(l) => {
            let _ = write!(out, "{l}");
        }
        OscType::Float(f) => out.push_str(&super::fmt_float(f64::from(*f))),
        OscType::Double(d) => out.push_str(&super::fmt_float(*d)),
        OscType::Bool(b) => {
            let _ = write!(out, "{b}");
        }
        OscType::String(s) => {
            // Only take what could possibly survive truncation.
            let s: String = s.chars().take(RENDER_BUDGET).collect();
            let _ = write!(out, "\"{s}\"");
        }
        OscType::Char(c) => {
            let _ = write!(out, "'{c}'");
        }
        OscType::Blob(b) => out.push_str(&sanitize::blob_preview(b)),
        OscType::Time(t) => {
            let _ = write!(out, "time {}.{:08x}", t.seconds, t.fractional);
        }
        OscType::Color(c) => {
            let _ = write!(out, "rgba #{:02x}{:02x}{:02x}{:02x}", c.red, c.green, c.blue, c.alpha);
        }
        OscType::Midi(m) => {
            let _ = write!(out, "midi {:02x} {:02x} {:02x} {:02x}", m.port, m.status, m.data1, m.data2);
        }
        OscType::Nil => out.push_str("nil"),
        OscType::Inf => out.push_str("inf"),
        OscType::Array(a) => {
            out.push('[');
            if depth < MAX_ARRAY_DEPTH {
                for (i, x) in a.content.iter().enumerate() {
                    if out.len() >= RENDER_BUDGET {
                        break;
                    }
                    if i > 0 {
                        out.push_str(", ");
                    }
                    render(out, x, depth + 1);
                }
            }
            out.push(']');
        }
    }
}

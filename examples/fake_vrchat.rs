//! Dev-only fake VRChat: sends OSC to 127.0.0.1 so the monitor can be tested
//! without VRChat. Not part of the main binary.
//!
//!     cargo run --release --example fake_vrchat -- [--port 9001] [--seconds 30]
//!
//! Simulates:
//! - `/avatar/change` at start, and a second one halfway through
//! - Velocity floats at ~60 Hz (VelocityY spikes briefly every 5 s)
//! - a bool that blips on for 50 ms every 3 s
//! - an int that steps once per second
//!
//! After the second avatar change, the stepping int stops (it belonged to the
//! first avatar) and a different bool starts toggling slowly.

#![forbid(unsafe_code)]

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use rosc::{OscMessage, OscPacket, OscType, encoder};

const AVATAR_A: &str = "avtr_00000000-1111-2222-3333-444444444444";
const AVATAR_B: &str = "avtr_55555555-6666-7777-8888-999999999999";
const TICK: Duration = Duration::from_micros(16_667); // ~60 Hz

struct Sender {
    socket: UdpSocket,
    target: SocketAddrV4,
    sent: u64,
}

impl Sender {
    fn send(&mut self, addr: &str, arg: OscType) {
        let packet = OscPacket::Message(OscMessage { addr: addr.to_string(), args: vec![arg] });
        let bytes = encoder::encode(&packet).expect("encodable message");
        // Ignore send errors (e.g. nothing listening yet); keep simulating.
        let _ = self.socket.send_to(&bytes, self.target);
        self.sent += 1;
    }

    fn param(&mut self, name: &str, arg: OscType) {
        self.send(&format!("/avatar/parameters/{name}"), arg);
    }
}

fn main() -> std::io::Result<()> {
    let mut port: u16 = 9001;
    let mut seconds: u64 = 30;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let value = args.next().unwrap_or_default();
        match a.as_str() {
            "--port" => port = value.parse().expect("--port <1-65535>"),
            "--seconds" => seconds = value.parse().expect("--seconds <N>"),
            _ => {
                eprintln!("usage: fake_vrchat [--port 9001] [--seconds 30]");
                std::process::exit(2);
            }
        }
    }

    // Loopback only, on both ends.
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let mut out = Sender { socket, target, sent: 0 };
    println!("fake_vrchat: sending to {target} for {seconds} s");

    let start = Instant::now();
    let total = Duration::from_secs(seconds);
    let switch_at = total / 2;
    let mut switched = false;
    let mut tick: u64 = 0;
    let mut step: i32 = 0;
    let mut blink_on_since: Option<Instant> = None;

    out.send("/avatar/change", OscType::String(AVATAR_A.into()));
    out.param("Blink", OscType::Bool(false));
    out.param("Step", OscType::Int(step));

    while start.elapsed() < total {
        let t = start.elapsed();
        let secs = t.as_secs_f64();

        // Velocity floats every frame; a short spike on VelocityY every 5 s.
        let spike = (secs % 5.0) < 0.05;
        out.param("VelocityX", OscType::Float((secs * 0.7).sin() as f32 * 1.5));
        out.param("VelocityY", OscType::Float(if spike { -9.5 } else { (secs * 1.3).cos() as f32 * 0.2 }));
        out.param("VelocityZ", OscType::Float((secs * 0.4).cos() as f32));
        out.param("VelocityMagnitude", OscType::Float(((secs * 0.7).sin().abs() * 1.5) as f32));

        // Bool blip: on for 50 ms every 3 s.
        match blink_on_since {
            None if tick > 0 && tick.is_multiple_of(180) => {
                out.param("Blink", OscType::Bool(true));
                blink_on_since = Some(Instant::now());
            }
            Some(on) if on.elapsed() >= Duration::from_millis(50) => {
                out.param("Blink", OscType::Bool(false));
                blink_on_since = None;
            }
            _ => {}
        }

        if !switched && t >= switch_at {
            switched = true;
            out.send("/avatar/change", OscType::String(AVATAR_B.into()));
            // The new avatar re-sends its parameters; Step isn't one of them.
            out.param("Blink", OscType::Bool(false));
            out.param("HatToggle", OscType::Bool(false));
        }

        if tick > 0 && tick.is_multiple_of(60) {
            if switched {
                out.param("HatToggle", OscType::Bool((tick / 60) % 4 < 2));
            } else {
                step = (step + 1) % 8;
                out.param("Step", OscType::Int(step));
            }
        }

        tick += 1;
        let next = start + TICK * u32::try_from(tick).unwrap_or(u32::MAX);
        thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    println!("fake_vrchat: done, sent {} messages", out.sent);
    Ok(())
}

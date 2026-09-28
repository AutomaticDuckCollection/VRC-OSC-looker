//! The one and only socket: a UDP receiver bound to 127.0.0.1.
//!
//! This module never sends anything. It binds with plain `UdpSocket::bind`
//! (no SO_REUSEADDR / SO_REUSEPORT), drops anything that isn't from
//! loopback, timestamps each datagram on arrival and hands it to the main
//! thread through a bounded channel. If the channel is full, the packet is
//! dropped and counted; the receiver never blocks on the UI.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Largest possible UDP payload fits in this.
const RECV_BUFFER: usize = 64 * 1024;
/// Packets buffered between receiver and UI (~1 s of very heavy VRChat traffic).
const QUEUE_CAPACITY: usize = 1024;
/// How often the receiver wakes to check for shutdown.
const RECV_TIMEOUT: Duration = Duration::from_millis(100);

pub struct Packet {
    pub at: Instant,
    pub data: Vec<u8>,
}

#[derive(Default)]
pub struct Counters {
    /// Dropped because the UI queue was full.
    pub queue_full: AtomicU64,
    /// Dropped because the source wasn't a loopback address.
    pub non_loopback: AtomicU64,
}

impl Counters {
    pub fn queue_full(&self) -> u64 {
        self.queue_full.load(Ordering::Relaxed)
    }

    pub fn non_loopback(&self) -> u64 {
        self.non_loopback.load(Ordering::Relaxed)
    }
}

/// Binds the listening socket on 127.0.0.1 only.
pub fn bind(port: u16) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))?;
    socket.set_read_timeout(Some(RECV_TIMEOUT))?;
    Ok(socket)
}

pub struct Listener {
    pub packets: Receiver<Packet>,
    pub counters: Arc<Counters>,
    pub local: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Listener {
    pub fn spawn(socket: UdpSocket) -> io::Result<Self> {
        let local = socket.local_addr()?;
        let (tx, rx) = mpsc::sync_channel(QUEUE_CAPACITY);
        let counters = Arc::new(Counters::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (counters, stop) = (counters.clone(), stop.clone());
            thread::Builder::new()
                .name("osc-receiver".into())
                .spawn(move || receive_loop(&socket, &tx, &counters, &stop))?
        };
        Ok(Self { packets: rx, counters, local, stop, thread: Some(thread) })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn receive_loop(socket: &UdpSocket, tx: &SyncSender<Packet>, counters: &Counters, stop: &AtomicBool) {
    let mut buf = vec![0u8; RECV_BUFFER];
    while !stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((len, src)) => {
                let at = Instant::now();
                if !src.ip().is_loopback() {
                    counters.non_loopback.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                match tx.try_send(Packet { at, data: buf[..len].to_vec() }) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        counters.queue_full.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Disconnected(_)) => return,
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                ) => {}
            // Anything else (e.g. a Windows WSAECONNRESET quirk): keep going,
            // but don't spin.
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

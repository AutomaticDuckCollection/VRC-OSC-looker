//! vrc-osc-looker: a read-only, local-only VRChat OSC debug monitor.
//!
//! One UDP socket bound to 127.0.0.1, no outbound traffic, nothing written to
//! disk except an explicit `e` export. See README.md.

#![forbid(unsafe_code)]

mod args;
mod export;
mod net;
mod state;
mod ui;

use std::io::{self, BufRead, Stdout, Write};
use std::process::ExitCode;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use crossterm::{cursor, execute};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::args::Command;
use crate::state::{Config, Store};

const FRAME: Duration = Duration::from_millis(50); // ~20 fps
/// Packets applied per loop iteration before the UI gets a turn.
const MAX_PACKETS_PER_TICK: usize = 4096;

fn main() -> ExitCode {
    let args = match args::parse(std::env::args_os().skip(1)) {
        Ok(Command::Run(a)) => a,
        Ok(Command::Help) => {
            print!("{}", args::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Command::Version) => {
            println!("vrc-osc-looker {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => return fail(&format!("error: {e}\n\n{}", args::USAGE)),
    };

    let socket = match net::bind(args.port) {
        Ok(s) => s,
        Err(e) => return fail(&bind_error(args.port, &e)),
    };
    let listener = match net::Listener::spawn(socket) {
        Ok(l) => l,
        Err(e) => return fail(&format!("error: could not start the receiver thread: {e}")),
    };

    let cfg = Config { blip_threshold: Duration::from_millis(args.blip_ms), ignore_prefixes: args.ignore };
    let store = Store::new(cfg, Instant::now());
    let app = ui::App::new(store, listener.local.to_string(), listener.counters.clone(), args.export_raw);

    match run(app, &listener) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&format!("error: {e}")),
    }
}

/// Prints a startup/runtime error and waits for Enter, so a window opened
/// by double-clicking the .exe doesn't vanish before it can be read.
fn fail(msg: &str) -> ExitCode {
    let mut err = io::stderr();
    let _ = writeln!(err, "{msg}\n\nPress Enter to exit.");
    let _ = err.flush();
    let _ = io::stdin().lock().read_line(&mut String::new());
    ExitCode::FAILURE
}

fn bind_error(port: u16, e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied => format!(
            "Could not listen on UDP 127.0.0.1:{port}: {e}\n\
             \n\
             Only one app can own an OSC port. Another OSC app is probably running\n\
             (OSC router, face tracking, chatbox tool, ...). Either close it, or run\n\
             this monitor on a different port:\n\
             \n\
             \x20   vrc-osc-looker --port 9002\n\
             \n\
             and start VRChat with the launch option:\n\
             \n\
             \x20   --osc=9000:127.0.0.1:9002"
        ),
        _ => format!("Could not listen on UDP 127.0.0.1:{port}: {e}"),
    }
}

/// Leaves raw mode and the alternate screen. Safe to call more than once.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
}

/// Restores the terminal when dropped (normal exit, error, or unwinding).
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // A panic inside the OSC parser is caught and counted as a malformed
        // packet; don't touch the terminal or print anything for it.
        if state::decode::panic_is_contained() {
            return;
        }
        restore_terminal();
        default_hook(info);
    }));
}

fn run(mut app: ui::App, listener: &net::Listener) -> io::Result<()> {
    install_panic_hook();
    enable_raw_mode()?;
    let guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
    let mut terminal: Terminal<CrosstermBackend<Stdout>> = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;

    let mut next_draw = Instant::now();
    let mut dirty = true;
    while !app.quit {
        for _ in 0..MAX_PACKETS_PER_TICK {
            match listener.packets.try_recv() {
                Ok(p) => app.ingest(&p.data, p.at),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(io::Error::other("the OSC receiver thread stopped unexpectedly"));
                }
            }
        }

        let now = Instant::now();
        if dirty || now >= next_draw {
            terminal.draw(|f| app.draw(f, now))?;
            next_draw = now + FRAME;
            dirty = false;
        }

        if event::poll(next_draw.saturating_duration_since(Instant::now()))? {
            match event::read()? {
                // Windows reports key releases too; act on presses and repeats only.
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    app.on_key(key, Instant::now());
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
    }
    drop(terminal);
    drop(guard);
    Ok(())
}

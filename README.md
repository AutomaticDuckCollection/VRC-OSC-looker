# vrc-osc-looker

A small terminal tool that shows everything VRChat sends over OSC, live, and
keeps the short-lived stuff you'd otherwise miss. It is read-only and local-only.

- **Live table**: every address seen this session, with its type, current value, time since last
  update, Hz, message count and change count. Floats also show session min/max and a
  peak-hold (largest absolute value in the last ~3 s), so spikes stay visible.
  Rows never disappear on their own; stale rows are dimmed; values that just
  changed are highlighted.
- **Blips**: a bool, int or string value that was replaced within 500 ms
  (`--blip-ms`) is logged with how long it held and when. Re-sending the same
  value doesn't count as a change.
- **History**: select a row and press Enter to see its last 64 changes (with how long each value held), the
  distinct values seen with their counts, and a sparkline for floats.
- **Avatar switches**: `/avatar/change` puts a marker in the event log.
  Rows last seen under a previous avatar are tagged `prev`. Nothing is deleted.
- **Noise filter**: on by default. It hides the per-frame built-ins (Velocity\*,
  AngularY, Upright, Voice, Viseme, GestureLeft/RightWeight) and any
  `--ignore` prefix. Hidden addresses are still recorded.

## Run it

```
cargo run --release
```

The executable ends up at `target\release\vrc-osc-looker.exe` (on Linux,
`target/release/vrc-osc-looker`). It's a single file: copy it anywhere and
double-click it. No install, no admin rights, no config files.

## Enable OSC in VRChat

In VRChat: open the **Action Menu → Options → OSC → Enabled**.
If a parameter you expect never shows up, use **Reset Config** in the same
menu. This makes VRChat regenerate the avatar's OSC config.

## Port 9001

VRChat sends to `127.0.0.1:9001` by default, and only one app can listen on
that port. If another OSC app (router, face tracking, chatbox tool...) already
has it, this tool tells you so on startup. Either close that app, or run this
tool on another port:

```
vrc-osc-looker --port 9002
```

and start VRChat with the launch option `--osc=9000:127.0.0.1:9002`.

## Keys

| Key | Action |
| --- | --- |
| `q` / Ctrl+C | quit |
| `p` | pause the view (capture continues in the background) |
| `/` | filter by address (Enter keeps it, Esc clears it) |
| `s` | cycle sort: name / last update / count |
| `Tab` / Shift+Tab | switch pane: Live / Blips / Events |
| `↑` `↓` PgUp PgDn Home End | move selection |
| `Enter` | open/close history for the selected row, blip or event |
| `n` | toggle the noise filter |
| `c` | clear all recorded data (asks first) |
| `e` | export a JSON snapshot to the current directory |
| `Esc` | close history / clear filter |

## Flags

| Flag | Meaning |
| --- | --- |
| `--port <PORT>` | UDP port on 127.0.0.1 to listen on (default 9001) |
| `--blip-ms <MS>` | blip threshold in ms (default 500) |
| `--ignore <PREFIX>` | also hide addresses starting with PREFIX under the noise filter (repeatable) |
| `--export-raw` | don't redact VRChat IDs in exports |
| `-h`, `--help` / `-V`, `--version` | help / version |

## Privacy guarantees

This is a passive listener that stays on your own machine.

- **Only your own PC.** It listens on `127.0.0.1` only, never on your
  network. Any packet that somehow arrives from a non-local address is dropped
  and counted.
- **It never sends anything.** It doesn't send OSC back to VRChat or forward it
  anywhere. There's no OSCQuery/mDNS, no DNS lookups, no update checks, no
  telemetry and no crash reporting. Its single receiving UDP socket is the only
  network socket it opens.
- **No servers.** There's no web page or local server of any kind, only the terminal.
- **Nothing written to disk.** No logs, caches, history or settings files.
  Received data is never printed outside the TUI. Closing the window forgets
  everything.
- **Except when you press `e`.** That writes one `osc-snapshot-NNN.json` in
  the current folder. It never overwrites an existing file, and it shows you
  the path. Times in it are "seconds since the tool started"; it contains no
  clock time, computer name or user name. Anything that looks like a VRChat ID
  (`avtr_…`, `usr_…`, `wrld_…`, `grp_…`) is replaced with `[redacted]` unless
  you started it with `--export-raw`.
- **It reads nothing else.** It doesn't read VRChat's files, logs or OSC config,
  any other file, your clipboard, or process/system information.
- **Hostile packets can't hurt it.** Every packet is treated as untrusted.
  Malformed ones are skipped and counted, and all storage is capped (4096
  addresses, 64 history entries and 32 distinct values per address, 1000
  blips, 500 events, 256 chars per text, 8 levels of bundle nesting). Control
  characters are shown escaped (`\x1b`), so a packet can't send escape codes to
  your terminal.
- **Small, auditable code.** It uses `#![forbid(unsafe_code)]` and has no build
  script. Its dependencies are `rosc`, `ratatui`, `crossterm` and `serde_json`,
  pinned in `Cargo.lock`. No HTTP, async-runtime, mDNS or telemetry crates are
  in the dependency tree.

## Testing without VRChat

```
cargo test
cargo run --release --example fake_vrchat -- [--port 9001] [--seconds 30]
```

`fake_vrchat` is a dev-only sender (not part of the main binary). It sends to
127.0.0.1 only and simulates:
- two `/avatar/change` messages
- Velocity floats at ~60 Hz, with a spike every 5 s
- a bool that blips on for 50 ms every 3 s
- a stepping int

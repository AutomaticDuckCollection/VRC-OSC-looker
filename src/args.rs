//! Command-line flags (hand-rolled; five flags don't justify a parser crate).

use std::ffi::OsString;

use crate::state::DEFAULT_BLIP_MS;

pub const DEFAULT_PORT: u16 = 9001;

#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    pub port: u16,
    pub blip_ms: u64,
    pub ignore: Vec<String>,
    pub export_raw: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(Args),
    Help,
    Version,
}

pub const USAGE: &str = "\
vrc-osc-looker - read-only, local-only VRChat OSC monitor

USAGE:
    vrc-osc-looker [OPTIONS]

OPTIONS:
    --port <PORT>        UDP port to listen on, on 127.0.0.1 [default: 9001]
    --blip-ms <MS>       Values replaced within this many ms are blips [default: 500]
    --ignore <PREFIX>    Hide addresses starting with PREFIX under the noise
                         filter (repeatable)
    --export-raw         Don't redact VRChat IDs (avtr_, usr_, wrld_, grp_) in exports
    -h, --help           Print this help
    -V, --version        Print version

KEYS:
    q quit   p pause   / filter   s sort   Tab pane   Up/Down select
    Enter history   n noise filter   c clear   e export
";

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut out = Args { port: DEFAULT_PORT, blip_ms: DEFAULT_BLIP_MS, ignore: Vec::new(), export_raw: false };
    let mut it = args.into_iter().map(|a| a.to_string_lossy().into_owned());
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            inline.clone().or_else(|| it.next()).ok_or_else(|| format!("{name} needs a value"))
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "--port" => {
                let v = value("--port")?;
                out.port = match v.parse::<u16>() {
                    Ok(p) if p > 0 => p,
                    _ => return Err(format!("invalid port '{v}' (expected 1-65535)")),
                };
            }
            "--blip-ms" => {
                let v = value("--blip-ms")?;
                out.blip_ms = match v.parse::<u64>() {
                    Ok(ms) if (1..=60_000).contains(&ms) => ms,
                    _ => return Err(format!("invalid --blip-ms '{v}' (expected 1-60000)")),
                };
            }
            "--ignore" => {
                let v = value("--ignore")?;
                if v.is_empty() {
                    return Err("--ignore needs a non-empty prefix".into());
                }
                out.ignore.push(v);
            }
            "--export-raw" if inline.is_none() => out.export_raw = true,
            _ => return Err(format!("unknown argument '{arg}'")),
        }
    }
    Ok(Command::Run(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Command, String> {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn defaults() {
        let Command::Run(a) = p(&[]).unwrap() else { panic!() };
        assert_eq!(a, Args { port: 9001, blip_ms: 500, ignore: vec![], export_raw: false });
    }

    #[test]
    fn all_flags() {
        let got = p(&["--port", "9002", "--blip-ms=250", "--ignore", "/a", "--ignore=/b", "--export-raw"]).unwrap();
        let want = Args { port: 9002, blip_ms: 250, ignore: vec!["/a".into(), "/b".into()], export_raw: true };
        assert_eq!(got, Command::Run(want));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(p(&["--port", "0"]).is_err());
        assert!(p(&["--port", "70000"]).is_err());
        assert!(p(&["--port"]).is_err());
        assert!(p(&["--blip-ms", "0"]).is_err());
        assert!(p(&["--ignore", ""]).is_err());
        assert!(p(&["--bogus"]).is_err());
        assert!(p(&["--export-raw=yes"]).is_err());
        assert_eq!(p(&["-h"]), Ok(Command::Help));
    }
}

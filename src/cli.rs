use std::ffi::OsString;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const HELP: &str = "loopsie — small, resilient command loops

Usage:
  loopsie run [OPTIONS] -- COMMAND [ARGS...]
  loopsie ls
  loopsie logs [-f|--follow] NAME
  loopsie kill NAME | --all
  loopsie alias set NAME -- COMMAND [ARGS...]
  loopsie alias ls | show NAME | rm NAME

Run options:
  -n, --name NAME          Unique name (generated if omitted)
  -e, --every DURATION     Minimum interval between starts, without overlap
  -s, --sleep DURATION     Delay after each completion (default: 0)
  -m, --max N              Number of attempts (default: 0, forever)
      --alias NAME         Prepend a saved command
      --fg                 Stay in foreground; output still goes to logs
      --timeout DURATION   Limit each attempt (default: 1h; 0 disables)
      --grace DURATION     Allow termination before force kill (default: 5s)
      --backoff DURATION   Initial retry delay after failure (default: 1s)
      --max-backoff DUR    Cap exponential retry delay (default: 1m)
      --log-bytes N        Bytes per log file, keeping two (min: 4096; default: 5242880)

Durations: integer seconds, or ms/s/m/h, e.g. 100ms, 30s, 1h30m.
Names: 1–48 ASCII letters, digits, underscores or hyphens; no leading hyphen.
Commands inherit the working directory and environment; stdin is closed.
Each loop is an independent background process. Use agent CLIs in print/exec mode.
State: $LOOPSIE_DIR or ~/.loopsie. Stop with `loopsie kill NAME`.
";

#[derive(Debug)]
pub struct RunConfig {
    pub name: String,
    pub command: Vec<OsString>,
    pub every: Option<Duration>,
    pub sleep: Duration,
    pub max: u64,
    pub timeout: Duration,
    pub grace: Duration,
    pub backoff: Duration,
    pub max_backoff: Duration,
    pub log_bytes: u64,
    pub foreground: bool,
}

pub fn valid_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.starts_with('-')
        || name.len() > 48
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("names must be 1–48 ASCII letters, digits, underscores or hyphens, with no leading hyphen".into());
    }
    Ok(())
}

pub fn duration(input: &str) -> Result<Duration, String> {
    let invalid =
        || format!("invalid duration '{input}' (use 100ms, 30s, 5m, 1h30m or integer seconds)");
    if input.is_empty() {
        return Err(invalid());
    }
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut total = 0_u64;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if start == i {
            return Err(invalid());
        }
        let value = input[start..i].parse::<u64>().map_err(|_| invalid())?;
        let multiplier = if i == bytes.len() {
            1000
        } else if bytes[i..].starts_with(b"ms") {
            i += 2;
            1
        } else {
            let unit = bytes[i];
            i += 1;
            match unit {
                b's' => 1000,
                b'm' => 60_000,
                b'h' => 3_600_000,
                _ => return Err(invalid()),
            }
        };
        total = value
            .checked_mul(multiplier)
            .and_then(|v| total.checked_add(v))
            .ok_or_else(invalid)?;
    }
    // Keep all monotonic deadline arithmetic representable on supported systems.
    if total > 365 * 24 * 60 * 60 * 1000 {
        return Err("duration must be at most 365 days".into());
    }
    Ok(Duration::from_millis(total))
}

pub fn parse_run(args: &[OsString]) -> Result<(RunConfig, Option<String>), String> {
    let mut cfg = RunConfig {
        name: format!(
            "loop-{:x}-{:x}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ),
        command: Vec::new(),
        every: None,
        sleep: Duration::ZERO,
        max: 0,
        timeout: Duration::from_secs(3600),
        grace: Duration::from_secs(5),
        backoff: Duration::from_secs(1),
        max_backoff: Duration::from_secs(60),
        log_bytes: 5 * 1024 * 1024,
        foreground: false,
    };
    let mut alias = None;
    let mut sleep_set = false;
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].to_str().ok_or("option names must be UTF-8")?;
        i += 1;
        if flag == "--" {
            cfg.command.extend_from_slice(&args[i..]);
            break;
        }
        if flag == "--fg" {
            cfg.foreground = true;
            continue;
        }
        if !matches!(
            flag,
            "-n" | "--name"
                | "-e"
                | "--every"
                | "-s"
                | "--sleep"
                | "-m"
                | "--max"
                | "--alias"
                | "--timeout"
                | "--grace"
                | "--backoff"
                | "--max-backoff"
                | "--log-bytes"
        ) {
            return Err(format!("unknown option '{flag}'; put the command after --"));
        }
        let value = args
            .get(i)
            .and_then(|v| v.to_str())
            .ok_or_else(|| format!("{flag} needs a value"))?;
        i += 1;
        match flag {
            "-n" | "--name" => {
                valid_name(value)?;
                cfg.name = value.into();
            }
            "-e" | "--every" => cfg.every = Some(duration(value)?),
            "-s" | "--sleep" => {
                sleep_set = true;
                cfg.sleep = duration(value)?;
            }
            "-m" | "--max" => {
                cfg.max = value
                    .parse()
                    .map_err(|_| "--max needs a nonnegative integer")?
            }
            "--alias" => {
                valid_name(value)?;
                alias = Some(value.into());
            }
            "--timeout" => cfg.timeout = duration(value)?,
            "--grace" => cfg.grace = duration(value)?,
            "--backoff" => cfg.backoff = duration(value)?,
            "--max-backoff" => cfg.max_backoff = duration(value)?,
            "--log-bytes" => {
                cfg.log_bytes = value
                    .parse()
                    .map_err(|_| "--log-bytes needs a positive integer")?
            }
            _ => unreachable!(),
        }
    }
    if cfg.every.is_some() && sleep_set {
        return Err("cannot use both --every and --sleep".into());
    }
    if cfg.backoff.is_zero() || cfg.max_backoff < cfg.backoff {
        return Err(
            "--backoff must be positive and --max-backoff must be at least --backoff".into(),
        );
    }
    if cfg.log_bytes < 4096 {
        return Err("--log-bytes must be at least 4096".into());
    }
    if cfg.command.is_empty() && alias.is_none() {
        return Err("no command specified; use -- COMMAND or --alias NAME".into());
    }
    Ok((cfg, alias))
}

impl RunConfig {
    pub fn worker_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> =
            vec!["__worker".into(), "--name".into(), self.name.clone().into()];
        for (flag, value) in [
            ("--sleep", self.sleep),
            ("--timeout", self.timeout),
            ("--grace", self.grace),
            ("--backoff", self.backoff),
            ("--max-backoff", self.max_backoff),
        ] {
            if flag == "--sleep" && self.every.is_some() {
                continue;
            }
            args.extend([flag.into(), format!("{}ms", value.as_millis()).into()]);
        }
        if let Some(every) = self.every {
            args.extend(["--every".into(), format!("{}ms", every.as_millis()).into()]);
        }
        args.extend([
            "--max".into(),
            self.max.to_string().into(),
            "--log-bytes".into(),
            self.log_bytes.to_string().into(),
            "--".into(),
        ]);
        args.extend(self.command.iter().cloned());
        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durations_are_checked() {
        assert_eq!(duration("1h30m5s2ms").unwrap().as_millis(), 5_405_002);
        assert_eq!(duration("12").unwrap().as_secs(), 12);
        for bad in [
            "",
            "m",
            "-1s",
            "1.5s",
            "1d",
            "1µs",
            "999999999999999999999h",
        ] {
            assert!(duration(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn names_cannot_escape_state_directory() {
        for name in ["", ".", "..", "../x", "a/b", "a\nb", "é"] {
            assert!(valid_name(name).is_err());
        }
        assert!(valid_name("agent-1_test").is_ok());
    }
}

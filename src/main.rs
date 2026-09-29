#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("loopsie currently supports macOS and Linux");

mod cli;
mod engine;
mod platform;
mod state;

use std::error::Error;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let worker = args.first().is_some_and(|a| a == "__worker");
    let code = match dispatch(&args) {
        Ok(code) => code,
        Err(err) => {
            if worker {
                let _ = writeln!(io::stdout(), "ERROR {err}");
            }
            let _ = writeln!(io::stderr(), "loopsie: {err}");
            1
        }
    };
    std::process::exit(code.clamp(0, 255));
}

fn dispatch(args: &[OsString]) -> Result<i32> {
    let Some(command) = args.first().and_then(|s| s.to_str()) else {
        print!("{}", cli::HELP);
        return Ok(0);
    };
    if matches!(command, "-h" | "--help" | "help")
        || (command == "run"
            && args[1..]
                .iter()
                .take_while(|v| *v != "--")
                .any(|v| v == "--help" || v == "-h"))
    {
        print!("{}", cli::HELP);
        return Ok(0);
    }
    if matches!(command, "-V" | "--version") {
        println!("loopsie {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    let state = state::State::new()?;
    match command {
        "run" | "__worker" => {
            let (mut config, alias) = cli::parse_run(&args[1..])?;
            if let Some(name) = alias {
                let mut prefix = state
                    .alias(&name)
                    .map_err(|err| format!("alias '{name}': {err}"))?;
                prefix.append(&mut config.command);
                config.command = prefix;
            }
            if config.command.first().is_none_or(|v| v.is_empty()) {
                return Err("command must not be empty".into());
            }
            if state.path(&config.name, "pid").exists() {
                return Err(format!(
                    "legacy PID file for '{}': stop the old loop and remove its .pid file first",
                    config.name
                )
                .into());
            }
            state.ensure()?;
            if command == "__worker" || config.foreground {
                engine::run(&config, &state, command == "__worker")
            } else {
                launch(&config, &state)?;
                Ok(0)
            }
        }
        "ls" if args.len() == 1 => {
            list(&state)?;
            Ok(0)
        }
        "kill" if args.len() == 2 => {
            if args[1] == "--all" {
                for name in state.names()? {
                    if state.is_running(&name)? {
                        stop(&state, &name)?;
                    }
                }
            } else {
                stop(&state, name(&args[1])?)?;
            }
            Ok(0)
        }
        "logs" => {
            let mut follow = false;
            let mut target = None;
            for arg in &args[1..] {
                if arg == "-f" || arg == "--follow" {
                    follow = true;
                } else if target.is_none() {
                    target = Some(name(arg)?);
                } else {
                    return Err("usage: loopsie logs [-f] NAME".into());
                }
            }
            logs(
                &state,
                target.ok_or("usage: loopsie logs [-f] NAME")?,
                follow,
            )?;
            Ok(0)
        }
        "alias" => {
            aliases(&state, &args[1..])?;
            Ok(0)
        }
        _ => Err("unknown command or invalid arguments; see loopsie --help".into()),
    }
}

fn name(arg: &OsString) -> Result<&str> {
    let value = arg.to_str().ok_or("name must be UTF-8")?;
    cli::valid_name(value)?;
    Ok(value)
}

fn launch(config: &cli::RunConfig, state: &state::State) -> Result<()> {
    // A socket gives startup acknowledgement a deadline and avoids a thread/pipe.
    let (read, write) = UnixStream::pair()?;
    read.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(config.worker_args())
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(write)))
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe; this callback touches no Rust locks or allocation.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    drop(command);
    let mut response = String::new();
    let read_result = BufReader::new(read).take(4096).read_line(&mut response);
    if read_result.is_ok() && response == "READY\n" {
        println!(
            "Started '{}' (pid {}). Logs: {}",
            config.name,
            child.id(),
            state.path(&config.name, "log").display()
        );
        return Ok(());
    }
    // Only stop the child we created. A failed lock acquisition may mean this
    // name belongs to another loop. The unreaped child PID cannot be reused.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let deadline = Instant::now() + config.grace + Duration::from_secs(1);
    while child.try_wait()?.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait()?.is_none() {
        child.kill()?;
        child.wait()?;
    }
    if let Err(err) = read_result {
        return Err(format!("background startup failed: {err}").into());
    }
    Err(format!(
        "background startup failed: {}",
        response
            .trim()
            .strip_prefix("ERROR ")
            .unwrap_or(response.trim())
    )
    .into())
}

fn list(state: &state::State) -> Result<()> {
    let names = state.names()?;
    if names.is_empty() {
        println!("No loops.");
        return Ok(());
    }
    println!(
        "{:<24} {:<10} {:<8} {:<10} {:<10} EXIT",
        "NAME", "STATUS", "PID", "ITERATION", "FAILURES"
    );
    for name in names {
        let text = fs::read_to_string(state.path(&name, "status")).unwrap_or_default();
        let fields: Vec<_> = text.trim().split('\t').collect();
        let get = |i| fields.get(i).copied().unwrap_or("-");
        let alive = state.is_running(&name)?;
        let phase = if alive {
            get(1)
        } else if get(1) == "stopped" {
            "stopped"
        } else {
            "stale"
        };
        println!(
            "{name:<24} {phase:<10} {:<8} {:<10} {:<10} {}",
            get(0),
            get(2),
            get(3),
            get(4)
        );
    }
    Ok(())
}

fn stop(state: &state::State, name: &str) -> Result<()> {
    if !state.is_running(name)? {
        println!("'{name}' is not running.");
        return Ok(());
    }
    // PID files are never used for signalling: only a live instance receives this request.
    let mut stream = match UnixStream::connect(state.path(name, "sock")) {
        Ok(stream) => stream,
        Err(_) if !state.is_running(name)? => {
            println!("'{name}' has stopped.");
            return Ok(());
        }
        Err(err) => return Err(format!("cannot contact '{name}': {err}").into()),
    };
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"STOP\n")?;
    let mut reply = String::new();
    BufReader::new(stream).take(32).read_line(&mut reply)?;
    if reply != "OK\n" {
        return Err(format!("unexpected stop response from '{name}'").into());
    }
    println!("Stopping '{name}'.");
    Ok(())
}

fn aliases(state: &state::State, args: &[OsString]) -> Result<()> {
    match args.first().and_then(|v| v.to_str()) {
        Some("ls") if args.len() == 1 => {
            for name in state.alias_names()? {
                println!("{name}: {}", display_command(&state.alias(&name)?));
            }
        }
        Some("show") if args.len() == 2 => {
            println!("{}", display_command(&state.alias(name(&args[1])?)?))
        }
        Some("rm") if args.len() == 2 => {
            let name = name(&args[1])?;
            state.remove_alias(name)?;
            println!("Alias '{name}' removed.");
        }
        Some("set") if args.len() >= 4 && args[2] == "--" => {
            let name = name(&args[1])?;
            if args[3].is_empty() {
                return Err("alias command must not be empty".into());
            }
            state.set_alias(name, &args[3..])?;
            println!("Alias '{name}' set.");
        }
        _ => {
            return Err(
                "usage: loopsie alias set NAME -- COMMAND [ARGS...] | ls | show NAME | rm NAME"
                    .into(),
            );
        }
    }
    Ok(())
}

fn display_command(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| {
            let s = arg.to_string_lossy();
            if !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_./-".contains(c))
            {
                s.into_owned()
            } else {
                format!("'{}'", s.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn logs(state: &state::State, name: &str, follow: bool) -> Result<()> {
    let path = state.path(name, "log");
    // Open the archive first, so a concurrent rotation cannot make both
    // descriptors refer to the same generation.
    let previous = if follow {
        None
    } else {
        match File::open(state.path(name, "log.1")) {
            Ok(file) => Some(file),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(err.into()),
        }
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut file = loop {
        match File::open(&path) {
            Ok(file) => break file,
            Err(err)
                if err.kind() == io::ErrorKind::NotFound
                    && Instant::now() < deadline
                    && state.is_running(name)? =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(err) => return Err(err.into()),
        }
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if !follow {
        // Snapshot both retained generations; bound the copy even while the loop writes.
        if let Some(previous) = previous {
            let size = previous.metadata()?.len();
            io::copy(&mut previous.take(size), &mut out)?;
        }
        let size = file.metadata()?.len();
        io::copy(&mut file.take(size), &mut out)?;
        return Ok(());
    }
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => {}
            Ok(count) => {
                out.write_all(&buffer[..count])?;
                out.flush()?;
                continue;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err.into()),
        }
        if let Ok(metadata) = fs::metadata(&path) {
            let old = file.metadata()?;
            if (old.dev(), old.ino()) != (metadata.dev(), metadata.ino()) {
                match File::open(&path) {
                    Ok(next) => {
                        file = next;
                        continue;
                    }
                    // Rotation briefly removes the current path. Keep the old
                    // descriptor and retry instead of exiting the follower.
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err.into()),
                }
            } else if metadata.len() < file.stream_position()? {
                file.rewind()?;
                continue;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

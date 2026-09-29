use crate::cli::RunConfig;
use crate::platform::{self, Signals};
use crate::state::State;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const OUTPUT_CHUNK: usize = 16 * 1024;
const OUTPUT_BATCH: usize = 16;
const MAX_CLIENTS: usize = 32;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);

struct SocketPath(PathBuf);

impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

struct Status<'a> {
    state: &'a State,
    name: &'a str,
    iteration: u64,
    failures: u64,
    exit: Option<i32>,
}

impl Status<'_> {
    fn write(&self, phase: &str) -> io::Result<()> {
        self.state.write_status(
            self.name,
            &format!(
                "{}\t{phase}\t{}\t{}\t{}\n",
                std::process::id(),
                self.iteration,
                self.failures,
                self.exit
                    .map_or_else(|| "-".into(), |code| code.to_string())
            ),
        )
    }
}

impl Drop for Status<'_> {
    fn drop(&mut self) {
        // Keep a useful final record even when an I/O error aborts supervision.
        let _ = self.write("stopped");
    }
}

struct RotatingLog {
    path: PathBuf,
    previous: PathBuf,
    file: File,
    bytes: u64,
    limit: u64,
}

impl RotatingLog {
    fn open(state: &State, config: &RunConfig) -> io::Result<Self> {
        let path = state.path(&config.name, "log");
        let previous = state.path(&config.name, "log.1");
        let file = Self::open_file(&path)?;
        let bytes = file.metadata()?.len();
        let mut result = Self {
            path,
            previous,
            file,
            bytes,
            limit: config.log_bytes,
        };
        if result.limit > 0 {
            // A smaller cap on a later run also bounds any existing archive.
            match OpenOptions::new().write(true).open(&result.previous) {
                Ok(old) if old.metadata()?.len() > result.limit => old.set_len(result.limit)?,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            if result.bytes > result.limit {
                result.file.set_len(result.limit)?;
                result.rotate()?;
            }
        }
        Ok(result)
    }

    fn open_file(path: &PathBuf) -> io::Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }

    fn rotate(&mut self) -> io::Result<()> {
        fs::rename(&self.path, &self.previous)?;
        self.file = Self::open_file(&self.path)?;
        self.bytes = 0;
        Ok(())
    }

    fn write(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            if self.limit > 0 && self.bytes >= self.limit {
                self.rotate()?;
            }
            let count = if self.limit == 0 {
                bytes.len()
            } else {
                bytes
                    .len()
                    .min((self.limit - self.bytes).min(usize::MAX as u64) as usize)
            };
            self.file.write_all(&bytes[..count])?;
            self.bytes += count as u64;
            bytes = &bytes[count..];
        }
        Ok(())
    }

    fn event(&mut self, message: std::fmt::Arguments<'_>) -> io::Result<()> {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        self.write(
            format!(
                "loopsie: {}.{:03} {message}\n",
                time.as_secs(),
                time.subsec_millis()
            )
            .as_bytes(),
        )
    }
}

struct Client {
    stream: UnixStream,
    input: [u8; 16],
    used: usize,
    accepted: Instant,
}

struct Control {
    listener: UnixListener,
    signals: Signals,
    clients: Vec<Client>,
    stop: Option<i32>,
}

impl Control {
    fn service(&mut self) -> io::Result<()> {
        let mut wake = [0_u8; 256];
        for _ in 0..16 {
            match self.signals.reader.read(&mut wake) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        // Inspect the flag after draining its wake bytes. A later signal leaves
        // a byte for poll, so a shutdown request cannot be lost between them.
        if let Some(signal) = self.signals.received() {
            self.stop.get_or_insert(128 + signal);
        }
        // Bound each pass: an abusive socket client cannot starve timeouts.
        for _ in 0..MAX_CLIENTS {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if self.clients.len() < MAX_CLIENTS {
                        stream.set_nonblocking(true)?;
                        self.clients.push(Client {
                            stream,
                            input: [0; 16],
                            used: 0,
                            accepted: Instant::now(),
                        });
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        let mut index = 0;
        while index < self.clients.len() {
            let client = &mut self.clients[index];
            let remove = match client.stream.read(&mut client.input[client.used..]) {
                Ok(0) => true,
                Ok(count) => {
                    client.used += count;
                    let input = &client.input[..client.used];
                    if input == b"STOP\n" || input == b"STOP\r\n" {
                        self.stop.get_or_insert(0);
                        let _ = client.stream.write_all(b"OK\n");
                        true
                    } else if input.contains(&b'\n') || client.used == client.input.len() {
                        let _ = client.stream.write_all(b"ERROR invalid request\n");
                        true
                    } else {
                        false
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => false,
                Err(_) => true,
            };
            if remove || client.accepted.elapsed() >= CLIENT_TIMEOUT {
                self.clients.swap_remove(index);
            } else {
                index += 1;
            }
        }
        Ok(())
    }

    fn poll(&self, output: Option<&UnixStream>, deadline: Option<Instant>) -> io::Result<()> {
        let mut fds = Vec::with_capacity(3 + self.clients.len());
        for fd in [self.listener.as_raw_fd(), self.signals.reader.as_raw_fd()] {
            fds.push(libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if let Some(output) = output {
            fds.push(libc::pollfd {
                fd: output.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let mut deadline = deadline;
        for client in &self.clients {
            fds.push(libc::pollfd {
                fd: client.stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            deadline = earlier(deadline, Some(client.accepted + CLIENT_TIMEOUT));
        }
        let timeout = deadline.map_or(-1, |deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            remaining
                .as_millis()
                .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0))
                .min(i32::MAX as u128) as i32
        });
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        Ok(())
    }

    fn wait_until(&mut self, deadline: Instant) -> io::Result<()> {
        loop {
            self.service()?;
            if self.stop.is_some() || Instant::now() >= deadline {
                return Ok(());
            }
            self.poll(None, Some(deadline))?;
        }
    }
}

struct ActiveChild {
    child: Child,
    reaped: bool,
}

impl Drop for ActiveChild {
    fn drop(&mut self) {
        if !self.reaped {
            // Error paths must not leave a command running after releasing the
            // instance lock. The leader is still reserved here by waitid.
            let _ = platform::signal_group(self.child.id(), libc::SIGKILL, false);
            let _ = self.child.wait();
        }
    }
}

fn drain_output(output: &mut UnixStream, log: &mut RotatingLog) -> io::Result<bool> {
    let mut buffer = [0_u8; OUTPUT_CHUNK];
    for _ in 0..OUTPUT_BATCH {
        match output.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => log.write(&buffer[..count])?,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn spawn(config: &RunConfig, iteration: u64) -> io::Result<(ActiveChild, UnixStream)> {
    let (reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    let stderr = writer.try_clone()?;
    let mut command = Command::new(&config.command[0]);
    command
        .args(&config.command[1..])
        .env("LOOPSIE_NAME", &config.name)
        .env("LOOPSIE_ITERATION", iteration.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .stderr(Stdio::from(OwnedFd::from(stderr)))
        .process_group(0);
    let child = command.spawn()?;
    Ok((
        ActiveChild {
            child,
            reaped: false,
        },
        reader,
    ))
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}

fn earlier(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn supervise(
    mut child: ActiveChild,
    mut output: UnixStream,
    config: &RunConfig,
    control: &mut Control,
    log: &mut RotatingLog,
    started: Instant,
) -> io::Result<i32> {
    let timeout = if config.timeout.is_zero() {
        None
    } else {
        started.checked_add(config.timeout)
    };
    let mut termination = None;
    let mut forced_exit = None;
    let mut killed = false;
    let mut eof = false;
    loop {
        control.service()?;
        if !eof {
            eof = drain_output(&mut output, log)?;
        }
        let exited = platform::child_exited(child.child.id())?;
        let now = Instant::now();
        if forced_exit.is_none() {
            if let Some(code) = control.stop {
                forced_exit = Some(code);
                log.event(format_args!("stop requested; terminating command"))?;
            } else if !exited && timeout.is_some_and(|deadline| now >= deadline) {
                forced_exit = Some(124);
                log.event(format_args!("command timed out; terminating process group"))?;
            }
        }
        if termination.is_none() && (exited || forced_exit.is_some()) {
            platform::signal_group(child.child.id(), libc::SIGTERM, exited)?;
            termination = Some(now.checked_add(config.grace).unwrap_or(now));
        }
        // EOF and an exited leader mean normal commands need no grace delay.
        // Kill any descendants which closed their output before reaping the
        // leader; retained PID ownership makes the group signal safe.
        if !killed && termination.is_some_and(|deadline| now >= deadline || (exited && eof)) {
            platform::signal_group(child.child.id(), libc::SIGKILL, exited)?;
            killed = true;
        }
        if killed && exited {
            // Snapshot the queued bytes so large socket buffers are drained
            // completely, while an escaped descendant cannot prolong cleanup
            // forever by retaining the descriptor or continuously writing.
            if !eof {
                let mut remaining = platform::pending_output(output.as_raw_fd())?;
                let mut buffer = [0_u8; OUTPUT_CHUNK];
                while remaining > 0 {
                    control.service()?;
                    let count = remaining.min(buffer.len());
                    match output.read(&mut buffer[..count]) {
                        Ok(0) => break,
                        Ok(count) => {
                            log.write(&buffer[..count])?;
                            remaining -= count;
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) => return Err(error),
                    }
                }
            }
            let status = child.child.wait()?;
            child.reaped = true;
            return Ok(forced_exit.unwrap_or_else(|| exit_code(status)));
        }
        let deadline = if killed {
            None
        } else if termination.is_some() {
            termination
        } else {
            timeout
        };
        control.poll(if eof { None } else { Some(&output) }, deadline)?;
    }
}

fn failure_delay(base: Duration, maximum: Duration, failures: u64) -> Duration {
    if failures == 0 || base.is_zero() {
        return Duration::ZERO;
    }
    let mut delay = base.min(maximum);
    for _ in 1..failures.min(128) {
        delay = delay.saturating_mul(2).min(maximum);
        if delay == maximum {
            break;
        }
    }
    delay
}

fn run_loop(
    config: &RunConfig,
    control: &mut Control,
    log: &mut RotatingLog,
    status: &mut Status<'_>,
) -> io::Result<i32> {
    loop {
        control.service()?;
        if let Some(code) = control.stop {
            return Ok(code);
        }
        status.iteration = status.iteration.saturating_add(1);
        status.write("running")?;
        log.event(format_args!("iteration {} started", status.iteration))?;
        let started = Instant::now();
        let code = match spawn(config, status.iteration) {
            Ok((child, output)) => supervise(child, output, config, control, log, started)?,
            Err(error) => {
                log.event(format_args!("cannot start command: {error}"))?;
                127
            }
        };
        status.exit = Some(code);
        log.event(format_args!(
            "iteration {} exited {code} after {}ms",
            status.iteration,
            started.elapsed().as_millis()
        ))?;
        status.failures = if code == 0 {
            0
        } else {
            status.failures.saturating_add(1)
        };
        if control.stop.is_some() || (config.max != 0 && status.iteration >= config.max) {
            return Ok(control.stop.unwrap_or(code));
        }
        status.write("waiting")?;
        let completed = Instant::now();
        let delay = config.sleep.max(failure_delay(
            config.backoff,
            config.max_backoff,
            status.failures,
        ));
        let mut next = completed.checked_add(delay).unwrap_or(completed);
        if let Some(every) = config.every {
            next = next.max(started.checked_add(every).unwrap_or(started));
        }
        if status.failures > 0 {
            log.event(format_args!(
                "{} consecutive failures; retry in {}ms",
                status.failures,
                next.saturating_duration_since(completed).as_millis()
            ))?;
        }
        control.wait_until(next)?;
    }
}

pub fn run(config: &RunConfig, state: &State, ready: bool) -> Result<i32, Box<dyn Error>> {
    let _lock = state.lock(&config.name)?;
    let socket = state.path(&config.name, "sock");
    match fs::remove_file(&socket) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(&socket)?;
    let _socket = SocketPath(socket);
    listener.set_nonblocking(true)?;
    let mut log = RotatingLog::open(state, config)?;
    let mut control = Control {
        listener,
        signals: Signals::install()?,
        clients: Vec::new(),
        stop: None,
    };
    let mut status = Status {
        state,
        name: &config.name,
        iteration: 0,
        failures: 0,
        exit: None,
    };
    let result = (|| -> io::Result<i32> {
        status.write("waiting")?;
        if ready {
            let mut stdout = io::stdout().lock();
            stdout.write_all(b"READY\n")?;
            stdout.flush()?;
        }
        let code = run_loop(config, &mut control, &mut log, &mut status)?;
        log.event(format_args!("stopped with exit {code}"))?;
        status.write("stopped")?;
        Ok(code)
    })();
    match result {
        Ok(code) => Ok(code),
        Err(error) => {
            status.exit = Some(1);
            status.failures = status.failures.saturating_add(1);
            let _ = log.event(format_args!("supervisor error: {error}; stopped"));
            let _ = status.write("stopped");
            Err(error.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::failure_delay;
    use std::time::Duration;

    #[test]
    fn failure_backoff_doubles_caps_and_resets() {
        let base = Duration::from_millis(10);
        let maximum = Duration::from_millis(35);
        assert_eq!(failure_delay(base, maximum, 0), Duration::ZERO);
        assert_eq!(failure_delay(base, maximum, 1), base);
        assert_eq!(failure_delay(base, maximum, 2), Duration::from_millis(20));
        assert_eq!(failure_delay(base, maximum, 3), maximum);
        assert_eq!(failure_delay(base, maximum, u64::MAX), maximum);
        assert_eq!(failure_delay(Duration::ZERO, maximum, 3), Duration::ZERO);
    }
}

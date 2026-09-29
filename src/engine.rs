use crate::cli::RunConfig;
use crate::platform::{self, Signals};
use crate::state::State;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const OUTPUT_CHUNK: usize = 16 * 1024;
const OUTPUT_BATCH: usize = 16;
const MAX_CLIENTS: usize = 32;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
const GATE_OUTPUT_LIMIT: usize = 16 * 1024;
const PROMPT_LIMIT: u64 = 1024 * 1024;
const RUN_OUTPUT_LIMIT: usize = 64 * 1024;

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

    fn poll(&self, outputs: &[&UnixStream], deadline: Option<Instant>) -> io::Result<()> {
        let mut fds = Vec::with_capacity(2 + outputs.len() + self.clients.len());
        for fd in [self.listener.as_raw_fd(), self.signals.reader.as_raw_fd()] {
            fds.push(libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        for output in outputs {
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
            self.poll(&[], Some(deadline))?;
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

struct Capture {
    bytes: Vec<u8>,
    limit: usize,
    tail: bool,
    truncated: bool,
}

impl Capture {
    fn new(limit: usize, tail: bool) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            tail,
            truncated: false,
        }
    }

    fn add(&mut self, chunk: &[u8]) {
        if chunk.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.truncated = true;
        }
        if self.tail {
            if chunk.len() >= self.limit {
                self.bytes.clear();
                self.bytes
                    .extend_from_slice(&chunk[chunk.len() - self.limit..]);
            } else {
                let overflow = self
                    .bytes
                    .len()
                    .saturating_add(chunk.len())
                    .saturating_sub(self.limit);
                self.bytes.drain(..overflow);
                self.bytes.extend_from_slice(chunk);
            }
        } else {
            let count = chunk.len().min(self.limit.saturating_sub(self.bytes.len()));
            self.bytes.extend_from_slice(&chunk[..count]);
        }
    }
}

fn drain_output(
    output: &mut UnixStream,
    log: &mut RotatingLog,
    mut capture: Option<&mut Capture>,
) -> io::Result<bool> {
    let mut buffer = [0_u8; OUTPUT_CHUNK];
    for _ in 0..OUTPUT_BATCH {
        match output.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                log.write(&buffer[..count])?;
                if let Some(capture) = capture.as_mut() {
                    capture.add(&buffer[..count]);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn spawn_stage(
    executable: &OsStr,
    args: &[OsString],
    config: &RunConfig,
    iteration: u64,
    env: &[(&str, &str)],
    stdin: Option<File>,
    split_stderr: bool,
) -> io::Result<(ActiveChild, Vec<UnixStream>)> {
    let (reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    let (stderr, extra_reader) = if split_stderr {
        let (extra_reader, extra_writer) = UnixStream::pair()?;
        extra_reader.set_nonblocking(true)?;
        (extra_writer, Some(extra_reader))
    } else {
        (writer.try_clone()?, None)
    };
    let mut command = Command::new(executable);
    command
        .args(args)
        .env("LOOPSIE_NAME", &config.name)
        .env("LOOPSIE_ITERATION", iteration.to_string())
        .envs(env.iter().copied())
        .stdin(stdin.map_or_else(Stdio::null, Stdio::from))
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .stderr(Stdio::from(OwnedFd::from(stderr)))
        .process_group(0);
    let child = command.spawn()?;
    let mut outputs = vec![reader];
    if let Some(extra_reader) = extra_reader {
        outputs.push(extra_reader);
    }
    Ok((
        ActiveChild {
            child,
            reaped: false,
        },
        outputs,
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
    mut outputs: Vec<UnixStream>,
    config: &RunConfig,
    control: &mut Control,
    log: &mut RotatingLog,
    timeout: Option<Instant>,
    mut capture: Option<&mut Capture>,
) -> io::Result<i32> {
    let mut termination = None;
    let mut forced_exit = None;
    let mut killed = false;
    let mut eof = vec![false; outputs.len()];
    loop {
        control.service()?;
        for (index, output) in outputs.iter_mut().enumerate() {
            if !eof[index] {
                eof[index] = drain_output(
                    output,
                    log,
                    if index == 0 {
                        capture.as_deref_mut()
                    } else {
                        None
                    },
                )?;
            }
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
        if !killed
            && termination
                .is_some_and(|deadline| now >= deadline || (exited && eof.iter().all(|v| *v)))
        {
            platform::signal_group(child.child.id(), libc::SIGKILL, exited)?;
            killed = true;
        }
        if killed && exited {
            // Snapshot the queued bytes so large socket buffers are drained
            // completely, while an escaped descendant cannot prolong cleanup
            // forever by retaining the descriptor or continuously writing.
            for (index, output) in outputs.iter_mut().enumerate() {
                if eof[index] {
                    continue;
                }
                let mut remaining = platform::pending_output(output.as_raw_fd())?;
                let mut buffer = [0_u8; OUTPUT_CHUNK];
                while remaining > 0 {
                    control.service()?;
                    let count = remaining.min(buffer.len());
                    match output.read(&mut buffer[..count]) {
                        Ok(0) => break,
                        Ok(count) => {
                            log.write(&buffer[..count])?;
                            if index == 0
                                && let Some(capture) = capture.as_deref_mut()
                            {
                                capture.add(&buffer[..count]);
                            }
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
        let active: Vec<_> = outputs
            .iter()
            .zip(&eof)
            .filter_map(|(output, done)| (!done).then_some(output))
            .collect();
        control.poll(&active, deadline)?;
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

struct Stage<'a> {
    executable: &'a OsStr,
    args: &'a [OsString],
    iteration: u64,
    env: &'a [(&'a str, &'a str)],
    stdin: Option<File>,
    split_stderr: bool,
    timeout: Duration,
    capture: Option<&'a mut Capture>,
    label: &'a str,
}

fn execute(
    stage: Stage<'_>,
    config: &RunConfig,
    control: &mut Control,
    log: &mut RotatingLog,
) -> io::Result<(i32, bool)> {
    let started = Instant::now();
    match spawn_stage(
        stage.executable,
        stage.args,
        config,
        stage.iteration,
        stage.env,
        stage.stdin,
        stage.split_stderr,
    ) {
        Ok((child, outputs)) => Ok((
            supervise(
                child,
                outputs,
                config,
                control,
                log,
                if stage.timeout.is_zero() {
                    None
                } else {
                    started.checked_add(stage.timeout)
                },
                stage.capture,
            )?,
            true,
        )),
        Err(error) => {
            log.event(format_args!("cannot start {}: {error}", stage.label))?;
            Ok((127, false))
        }
    }
}

fn prompt_stdin(
    config: &RunConfig,
    state: &State,
    iteration: u64,
    gate_output: &str,
) -> io::Result<Option<File>> {
    let Some(path) = &config.prompt_file else {
        return Ok(None);
    };
    let mut content = Vec::new();
    File::open(Path::new(path))?
        .take(PROMPT_LIMIT + 1)
        .read_to_end(&mut content)?;
    if content.len() as u64 > PROMPT_LIMIT {
        return Err(io::Error::other("prompt file exceeds 1 MiB"));
    }
    let text =
        String::from_utf8(content).map_err(|_| io::Error::other("prompt file must be UTF-8"))?;
    let rendered = text.replace("${LOOPSIE_GATE_OUTPUT}", gate_output);
    if rendered.len() as u64 > PROMPT_LIMIT {
        return Err(io::Error::other("rendered prompt exceeds 1 MiB"));
    }
    let temp = state.path(
        &config.name,
        &format!("prompt-{}-{iteration}", std::process::id()),
    );
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    fs::remove_file(&temp)?;
    file.write_all(rendered.as_bytes())?;
    file.seek(SeekFrom::Start(0))?;
    Ok(Some(file))
}

struct RunOutputFile(PathBuf);

impl RunOutputFile {
    fn new(state: &State, name: &str, capture: &Capture) -> io::Result<Self> {
        let path = state.path(name, "run-output");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        file.write_all(&capture.bytes)?;
        Ok(Self(path))
    }
}

impl Drop for RunOutputFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn wait_next(
    config: &RunConfig,
    control: &mut Control,
    log: &mut RotatingLog,
    status: &mut Status<'_>,
    started: Instant,
) -> io::Result<()> {
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
    control.wait_until(next)
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
        let started = Instant::now();
        let mut gate_output = String::new();
        if let Some(gate) = &config.gate {
            status.write("gating")?;
            let mut capture = Capture::new(GATE_OUTPUT_LIMIT, false);
            let (gate_code, _) = execute(
                Stage {
                    executable: gate,
                    args: &[],
                    iteration: status.iteration.saturating_add(1),
                    env: &[],
                    stdin: None,
                    split_stderr: true,
                    timeout: config.hook_timeout,
                    capture: Some(&mut capture),
                    label: "gate",
                },
                config,
                control,
                log,
            )?;
            if let Some(code) = control.stop {
                return Ok(code);
            }
            if gate_code == 1 {
                log.event(format_args!("gate skipped command"))?;
                status.failures = 0;
                wait_next(config, control, log, status, started)?;
                continue;
            }
            let invalid = capture.truncated
                || capture.bytes.contains(&0)
                || String::from_utf8(capture.bytes.clone()).is_err();
            if gate_code != 0 || invalid {
                let code = if gate_code == 0 { 125 } else { gate_code };
                if invalid && gate_code == 0 {
                    log.event(format_args!(
                        "gate output must be UTF-8, contain no NUL, and fit in 16 KiB"
                    ))?;
                }
                log.event(format_args!("gate failed with exit {code}"))?;
                status.exit = Some(code);
                status.failures = status.failures.saturating_add(1);
                wait_next(config, control, log, status, started)?;
                continue;
            }
            gate_output = String::from_utf8(capture.bytes).unwrap();
            gate_output = gate_output.trim_end_matches(['\n', '\r']).to_owned();
        }
        let iteration = status.iteration.saturating_add(1);
        let stdin = match prompt_stdin(config, status.state, iteration, &gate_output) {
            Ok(stdin) => stdin,
            Err(error) => {
                log.event(format_args!("cannot load prompt file: {error}"))?;
                status.exit = Some(125);
                status.failures = status.failures.saturating_add(1);
                wait_next(config, control, log, status, started)?;
                continue;
            }
        };
        status.iteration = iteration;
        status.write("running")?;
        log.event(format_args!("iteration {} started", status.iteration))?;
        let mut capture = Capture::new(RUN_OUTPUT_LIMIT, true);
        let (command_code, launched) = execute(
            Stage {
                executable: &config.command[0],
                args: &config.command[1..],
                iteration,
                env: &[("LOOPSIE_GATE_OUTPUT", &gate_output)],
                stdin,
                split_stderr: false,
                timeout: config.timeout,
                capture: config.postrun.as_ref().map(|_| &mut capture),
                label: "command",
            },
            config,
            control,
            log,
        )?;
        let mut code = command_code;
        if launched
            && control.stop.is_none()
            && let Some(postrun) = &config.postrun
        {
            let output_file = RunOutputFile::new(status.state, &config.name, &capture)?;
            let path = output_file.0.to_string_lossy().into_owned();
            let exit = command_code.to_string();
            let truncated = if capture.truncated { "1" } else { "0" };
            status.write("postrun")?;
            log.event(format_args!("postrun started for iteration {iteration}"))?;
            let (post_code, _) = execute(
                Stage {
                    executable: postrun,
                    args: &[],
                    iteration,
                    env: &[
                        ("LOOPSIE_GATE_OUTPUT", &gate_output),
                        ("LOOPSIE_EXIT_CODE", &exit),
                        ("LOOPSIE_RUN_OUTPUT_FILE", &path),
                        ("LOOPSIE_RUN_OUTPUT_TRUNCATED", truncated),
                    ],
                    stdin: None,
                    split_stderr: false,
                    timeout: config.hook_timeout,
                    capture: None,
                    label: "postrun",
                },
                config,
                control,
                log,
            )?;
            log.event(format_args!("postrun exited {post_code}"))?;
            if command_code == 0 && post_code != 0 {
                code = post_code;
            }
        }
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
        wait_next(config, control, log, status, started)?;
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

#![cfg(unix)]

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
const POLL: Duration = Duration::from_millis(10);

struct Fixture {
    root: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        // Keep socket paths short on macOS, where sockaddr_un has 104 bytes.
        let root = PathBuf::from(format!(
            "/tmp/lp-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let state = root.join("state");
        Self { root, state }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_loopsie"));
        command
            .args(args)
            .env("LOOPSIE_DIR", &self.state)
            .current_dir(&self.root);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        output(self.command(args), Duration::from_secs(10))
    }

    fn ok(&self, args: &[&str]) -> Output {
        let result = self.run(args);
        assert_success(&result);
        result
    }

    fn log(&self, name: &str) -> String {
        fs::read_to_string(self.state.join(format!("{name}.log"))).unwrap()
    }

    fn script(&self, name: &str, contents: &str) {
        let path = self.root.join(name);
        fs::write(&path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn status(&self, name: &str) -> Option<Vec<String>> {
        let text = fs::read_to_string(self.state.join(format!("{name}.status"))).ok()?;
        let fields: Vec<_> = text.trim_end().split('\t').map(str::to_owned).collect();
        (fields.len() >= 5).then_some(fields)
    }

    fn wait_phase(&self, name: &str, phase: &str) {
        wait_until(Duration::from_secs(5), || {
            self.status(name).is_some_and(|fields| fields[1] == phase)
        });
    }

    fn assert_finished(&self, name: &str, iterations: u64, exit: i32) {
        self.wait_phase(name, "stopped");
        let fields = self.status(name).unwrap();
        assert_eq!(fields[2], iterations.to_string(), "status: {fields:?}");
        assert_eq!(fields[4], exit.to_string(), "status: {fields:?}");
    }

    fn assert_recorded_children_dead(&self) {
        let pids = self.recorded_pids();
        assert!(
            !pids.is_empty(),
            "the command did not record any child PIDs"
        );
        wait_until(Duration::from_secs(5), || {
            pids.iter().all(|&pid| !process_alive(pid))
        });
    }

    fn recorded_pids(&self) -> Vec<i32> {
        fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("pid-"))
            .filter_map(|entry| fs::read_to_string(entry.path()).ok())
            .filter_map(|text| text.trim().parse().ok())
            .filter(|&pid| pid > 1)
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Cleanup also runs when an assertion panics, including failed startup.
        if self.state.exists() {
            let mut command = self.command(&["kill", "--all"]);
            command.stdout(Stdio::null()).stderr(Stdio::null());
            if let Ok(mut child) = command.spawn() {
                let deadline = Instant::now() + Duration::from_secs(3);
                while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                    thread::sleep(POLL);
                }
                let _ = child.kill();
                let _ = child.wait();
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                let active = fs::read_dir(&self.state)
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "lock"))
                    .filter_map(|entry| {
                        fs::OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(entry.path())
                            .ok()
                    })
                    .any(|file| file.try_lock().is_err());
                if !active {
                    break;
                }
                thread::sleep(POLL);
            }
        }
        for pid in self.recorded_pids() {
            if process_alive(pid) {
                // Each file is written by a process launched by this fixture.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct ChildGuard {
    child: Option<Child>,
    capture: PathBuf,
}

impl ChildGuard {
    fn spawn(mut command: Command) -> Self {
        let capture = PathBuf::from(format!(
            "/tmp/lp-c-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&capture).unwrap();
        // Files avoid pipe-buffer deadlocks and do not block if a broken
        // background startup accidentally leaves a descendant holding stdout.
        command
            .stdout(fs::File::create(capture.join("stdout")).unwrap())
            .stderr(fs::File::create(capture.join("stderr")).unwrap());
        let mut guard = Self {
            child: None,
            capture,
        };
        guard.child = Some(command.spawn().expect("could not start CLI"));
        guard
    }

    fn output(mut self, timeout: Duration) -> Output {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                self.child.take();
                return Output {
                    status,
                    stdout: fs::read(self.capture.join("stdout")).unwrap(),
                    stderr: fs::read(self.capture.join("stderr")).unwrap(),
                };
            }
            assert!(
                Instant::now() < deadline,
                "CLI did not finish within {timeout:?}"
            );
            thread::sleep(POLL);
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            // Give a foreground supervisor a chance to reap its process group.
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(1);
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(POLL);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.capture);
    }
}

fn output(command: Command, timeout: Duration) -> Output {
    ChildGuard::spawn(command).output(timeout)
}

fn assert_success(result: &Output) {
    assert!(
        result.status.success(),
        "exit {:?}\nstdout: {}\nstderr: {}",
        result.status.code(),
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "condition was not met within {timeout:?}"
        );
        thread::sleep(POLL);
    }
}

fn process_alive(pid: i32) -> bool {
    // A reparented zombie cannot run and is waiting for the system to reap it.
    #[cfg(target_os = "linux")]
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat"))
        && stat
            .rsplit_once(") ")
            .is_some_and(|(_, tail)| tail.starts_with('Z'))
    {
        return false;
    }
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn finite_loop_preserves_arguments_cwd_and_environment() {
    let fixture = Fixture::new();
    let mut command = fixture.command(&[
        "run", "--fg", "--name", "arguments", "--max", "3", "--sleep", "1ms",
        "--", "/bin/sh", "-c",
        "printf 'args:%s|%s|%s|%s|%s|%s|%s\\n' \"$PWD\" \"$LOOPSIE_TEST_VALUE\" \"$LOOPSIE_NAME\" \"$LOOPSIE_ITERATION\" \"$1\" \"$2\" \"$3\"",
        "test", "two words", "$(touch should-not-exist)", "",
    ]);
    command.env("LOOPSIE_TEST_VALUE", "inherited value");
    assert_success(&output(command, Duration::from_secs(5)));
    let log = fixture.log("arguments");
    let expected_cwd = fs::canonicalize(&fixture.root).unwrap();
    for iteration in 1..=3 {
        assert!(log.contains(&format!(
            "args:{}|inherited value|arguments|{iteration}|two words|$(touch should-not-exist)|",
            expected_cwd.display()
        )), "log: {log}");
    }
    assert!(!fixture.root.join("should-not-exist").exists());
    fixture.assert_finished("arguments", 3, 0);
}

#[test]
fn command_arguments_can_contain_non_utf8_bytes() {
    let fixture = Fixture::new();
    let mut command = fixture.command(&[
        "run",
        "--fg",
        "-n",
        "bytes",
        "-m",
        "1",
        "--",
        "/usr/bin/printf",
        "%s\\n",
    ]);
    let argument = vec![b'a', 0xff, b'z'];
    command.arg(OsString::from_vec(argument.clone()));
    assert_success(&output(command, Duration::from_secs(5)));
    let log = fs::read(fixture.state.join("bytes.log")).unwrap();
    assert!(log.windows(argument.len()).any(|bytes| bytes == argument));
}

#[test]
fn real_git_cli_runs_multiple_times() {
    let fixture = Fixture::new();
    let mut init = Command::new("git");
    init.args(["init", "--quiet"]).current_dir(&fixture.root);
    assert_success(&output(init, Duration::from_secs(5)));
    fixture.ok(&[
        "run",
        "--fg",
        "-n",
        "git",
        "-m",
        "3",
        "-e",
        "1ms",
        "--",
        "git",
        "rev-parse",
        "--is-inside-work-tree",
    ]);
    let log = fixture.log("git");
    assert_eq!(
        log.lines().filter(|line| *line == "true").count(),
        3,
        "{log}"
    );
    fixture.assert_finished("git", 3, 0);
}

#[test]
fn transient_command_failures_retry_and_recover() {
    let fixture = Fixture::new();
    fixture.ok(&[
        "run",
        "--fg",
        "-n",
        "recover",
        "-m",
        "3",
        "--backoff",
        "10ms",
        "--max-backoff",
        "20ms",
        "--",
        "/bin/sh",
        "-c",
        "case \"$LOOPSIE_ITERATION\" in 1|2) echo retry; exit 7;; *) echo recovered;; esac",
    ]);
    let log = fixture.log("recover");
    assert_eq!(
        log.lines().filter(|line| *line == "retry").count(),
        2,
        "{log}"
    );
    assert_eq!(
        log.lines().filter(|line| *line == "recovered").count(),
        1,
        "{log}"
    );
    fixture.assert_finished("recover", 3, 0);
}

#[test]
fn finite_failures_return_the_last_child_exit_code() {
    let fixture = Fixture::new();
    let result = fixture.run(&[
        "run",
        "--fg",
        "-n",
        "failure",
        "-m",
        "2",
        "--backoff",
        "1ms",
        "--",
        "/bin/sh",
        "-c",
        "echo failed; exit 7",
    ]);
    assert_eq!(result.status.code(), Some(7));
    fixture.assert_finished("failure", 2, 7);
}

#[test]
fn spawn_failures_are_retried_and_reported() {
    let fixture = Fixture::new();
    let result = fixture.run(&[
        "run",
        "--fg",
        "-n",
        "missing",
        "-m",
        "2",
        "--backoff",
        "1ms",
        "--",
        "/definitely/not/a/loopsie-test-command",
    ]);
    assert_eq!(result.status.code(), Some(127));
    fixture.assert_finished("missing", 2, 127);
}

#[test]
fn timeouts_kill_term_resistant_process_groups_and_retry() {
    let fixture = Fixture::new();
    let result = fixture.run(&[
        "run", "--fg", "-n", "timeout", "-m", "2", "--timeout", "150ms",
        "--grace", "50ms", "--backoff", "1ms", "--", "/bin/sh", "-c",
        "trap '' TERM; echo $$ > \"pid-parent-$LOOPSIE_ITERATION\"; /bin/sh -c 'trap \"\" TERM; echo $$ > \"pid-child-$LOOPSIE_ITERATION\"; while :; do sleep 1; done' & wait",
    ]);
    assert_eq!(result.status.code(), Some(124));
    fixture.assert_finished("timeout", 2, 124);
    assert_eq!(fixture.recorded_pids().len(), 4);
    fixture.assert_recorded_children_dead();
}

#[test]
fn successful_leaders_do_not_leave_grandchildren_running() {
    let fixture = Fixture::new();
    fixture.ok(&[
        "run",
        "--fg",
        "-n",
        "grandchildren",
        "-m",
        "2",
        "--grace",
        "50ms",
        "--",
        "/bin/sh",
        "-c",
        "sleep 30 & echo $! > \"pid-grandchild-$LOOPSIE_ITERATION\"; exit 0",
    ]);
    assert_eq!(fixture.recorded_pids().len(), 2);
    fixture.assert_recorded_children_dead();
    fixture.assert_finished("grandchildren", 2, 0);
}

#[test]
fn stopping_during_backoff_is_prompt() {
    let fixture = Fixture::new();
    fixture.ok(&[
        "run",
        "-n",
        "backoff",
        "--backoff",
        "30s",
        "--max-backoff",
        "30s",
        "--grace",
        "50ms",
        "--",
        "/bin/sh",
        "-c",
        "echo attempted; exit 1",
    ]);
    wait_until(Duration::from_secs(5), || {
        fixture
            .status("backoff")
            .is_some_and(|fields| fields[1] == "waiting" && fields[2] == "1")
    });
    let start = Instant::now();
    fixture.ok(&["kill", "backoff"]);
    fixture.wait_phase("backoff", "stopped");
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(fixture.status("backoff").unwrap()[2], "1");
}

#[test]
fn independent_background_loops_can_run_and_stop_concurrently() {
    let fixture = Fixture::new();
    let names = ["first", "second", "third", "fourth"];
    let starts: Vec<_> = names
        .iter()
        .map(|name| {
            ChildGuard::spawn(fixture.command(&[
                "run",
                "-n",
                name,
                "--sleep",
                "30s",
                "--grace",
                "50ms",
                "--",
                "/bin/echo",
                name,
            ]))
        })
        .collect();
    for start in starts {
        assert_success(&start.output(Duration::from_secs(5)));
    }
    for name in names {
        fixture.wait_phase(name, "waiting");
    }
    let result = fixture.ok(&["ls"]);
    let listing = String::from_utf8_lossy(&result.stdout);
    for name in names {
        assert!(listing.contains(name), "{listing}");
    }
    fixture.ok(&["kill", "first"]);
    fixture.wait_phase("first", "stopped");
    for name in &names[1..] {
        assert_eq!(fixture.status(name).unwrap()[1], "waiting");
    }
    fixture.ok(&["kill", "--all"]);
    for name in names {
        fixture.wait_phase(name, "stopped");
    }
}

#[test]
fn concurrent_duplicate_names_have_only_one_owner() {
    let fixture = Fixture::new();
    let args = [
        "run",
        "-n",
        "exclusive",
        "--grace",
        "50ms",
        "--",
        "/bin/sh",
        "-c",
        "while :; do echo tick; sleep 0.01; done",
    ];
    let first = ChildGuard::spawn(fixture.command(&args));
    let second = ChildGuard::spawn(fixture.command(&args));
    let a = first.output(Duration::from_secs(5));
    let b = second.output(Duration::from_secs(5));
    assert_ne!(
        a.status.success(),
        b.status.success(),
        "both starts had the same outcome: {a:?} {b:?}"
    );
    fixture.wait_phase("exclusive", "running");
    let initial_ticks = fixture
        .log("exclusive")
        .lines()
        .filter(|line| *line == "tick")
        .count();
    wait_until(Duration::from_secs(5), || {
        fixture
            .log("exclusive")
            .lines()
            .filter(|line| *line == "tick")
            .count()
            >= initial_ticks + 3
    });
    assert_eq!(fixture.status("exclusive").unwrap()[1], "running");
    fixture.ok(&["kill", "exclusive"]);
    fixture.wait_phase("exclusive", "stopped");
}

#[test]
fn prune_removes_only_loops_that_are_not_running() {
    let fixture = Fixture::new();
    fixture.ok(&[
        "run",
        "--fg",
        "-n",
        "done",
        "-m",
        "1",
        "--",
        "/bin/echo",
        "first-run",
    ]);
    fixture.ok(&[
        "run",
        "-n",
        "live",
        "--sleep",
        "30s",
        "--grace",
        "50ms",
        "--",
        "/bin/echo",
        "live",
    ]);
    fixture.wait_phase("live", "waiting");
    // A killed supervisor leaves stale state; legacy PID files belong to the user.
    fs::write(fixture.state.join("crashed.lock"), b"").unwrap();
    fs::write(
        fixture.state.join("crashed.status"),
        "1\trunning\t1\t0\t-\n",
    )
    .unwrap();
    fs::write(fixture.state.join("crashed.log"), b"old\n").unwrap();
    fs::write(fixture.state.join("crashed.pid"), b"1\n").unwrap();

    let result = fixture.ok(&["prune"]);
    assert_eq!(
        String::from_utf8_lossy(&result.stdout),
        "Removed 'crashed'.\nRemoved 'done'.\n"
    );
    let mut files: Vec<_> = fs::read_dir(&fixture.state)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|file| !file.starts_with("live."))
        .collect();
    files.sort();
    assert_eq!(files, ["crashed.pid"]);
    for suffix in ["lock", "log", "sock", "status"] {
        assert!(fixture.state.join(format!("live.{suffix}")).exists());
    }
    let listing = fixture.ok(&["ls"]);
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert!(listing.contains("live"), "{listing}");
    assert!(
        !listing.contains("done") && !listing.contains("crashed"),
        "{listing}"
    );
    assert_eq!(fixture.ok(&["prune"]).stdout, b"No stopped loops.\n");

    fixture.ok(&[
        "run",
        "--fg",
        "-n",
        "done",
        "-m",
        "1",
        "--",
        "/bin/echo",
        "second-run",
    ]);
    let log = fixture.log("done");
    assert!(
        log.contains("second-run") && !log.contains("first-run"),
        "{log}"
    );
    fixture.assert_finished("done", 1, 0);
    fixture.ok(&["kill", "live"]);
    fixture.wait_phase("live", "stopped");
}

#[test]
fn aliases_preserve_argument_boundaries_and_support_lifecycle() {
    let fixture = Fixture::new();
    fixture.ok(&[
        "alias",
        "set",
        "greeting",
        "--",
        "/usr/bin/printf",
        "hello:%s\\n",
    ]);
    let listing = fixture.ok(&["alias", "ls"]);
    assert!(String::from_utf8_lossy(&listing.stdout).contains("greeting"));
    let shown = fixture.ok(&["alias", "show", "greeting"]);
    assert!(String::from_utf8_lossy(&shown.stdout).contains("printf"));
    fixture.ok(&[
        "run",
        "--fg",
        "-n",
        "alias",
        "-m",
        "2",
        "--alias",
        "greeting",
        "--",
        " two words ",
    ]);
    let log = fixture.log("alias");
    assert_eq!(
        log.lines()
            .filter(|line| *line == "hello: two words ")
            .count(),
        2,
        "{log}"
    );
    assert_eq!(fixture.ok(&["logs", "alias"]).stdout, log.as_bytes());
    fixture.ok(&["alias", "rm", "greeting"]);
    assert!(!fixture.run(&["alias", "show", "greeting"]).status.success());
}

#[test]
fn large_command_output_is_rotated_and_bounded() {
    let fixture = Fixture::new();
    fixture.ok(&[
        "run", "--fg", "-n", "output", "-m", "1", "--log-bytes", "4096", "--",
        "/bin/sh", "-c",
        "i=0; while [ \"$i\" -lt 4096 ]; do printf '%064d\\n' 0; i=$((i+1)); done; echo FINAL_OUTPUT",
    ]);
    let current = fixture.state.join("output.log");
    let previous = fixture.state.join("output.log.1");
    assert!(previous.exists(), "output did not rotate");
    assert!(fs::metadata(current).unwrap().len() <= 4096);
    assert!(fs::metadata(previous).unwrap().len() <= 4096);
    assert!(fixture.log("output").contains("FINAL_OUTPUT"));
    fixture.assert_finished("output", 1, 0);
}

#[test]
fn malformed_options_are_rejected_before_creating_state() {
    let fixture = Fixture::new();
    let cases: &[&[&str]] = &[
        &["run", "--every", "1s", "--sleep", "1s", "--", "echo"],
        &["run", "--timeout", "nonsense", "--", "echo"],
        &["run", "--grace", "-1", "--", "echo"],
        &["run", "--max", "-1", "--", "echo"],
        &["run", "--max", "abc", "--", "echo"],
        &["run", "--name", "../escape", "--", "echo"],
        &["run", "--name", "a/b", "--", "echo"],
        &["run", "--name", "--all", "--", "echo"],
        &["run", "--name", "-f", "--", "echo"],
        &["run", "--log-bytes", "1", "--", "echo"],
        &["run", "--unknown-option", "--", "echo"],
        &["run", "--fg"],
    ];
    for args in cases {
        let result = fixture.run(args);
        assert!(
            !result.status.success(),
            "accepted malformed arguments: {args:?}"
        );
        assert!(
            !fixture.state.exists(),
            "created state for invalid arguments: {args:?}"
        );
    }
}

#[test]
fn stale_status_and_socket_cannot_kill_an_unrelated_process() {
    let fixture = Fixture::new();
    fs::create_dir(&fixture.state).unwrap();
    let mut sleep = Command::new("/bin/sleep");
    sleep.arg("30");
    let mut unrelated = ChildGuard::spawn(sleep);
    let pid = unrelated.child.as_ref().unwrap().id();
    fs::write(
        fixture.state.join("stale.status"),
        format!("{pid}\trunning\t1\t0\t0\n"),
    )
    .unwrap();
    fs::write(fixture.state.join("stale.lock"), b"").unwrap();
    let listener = UnixListener::bind(fixture.state.join("stale.sock")).unwrap();
    drop(listener);
    let _ = fixture.run(&["kill", "stale"]);
    assert!(
        unrelated
            .child
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none()
    );
}

#[test]
fn foreground_commands_receive_closed_stdin() {
    let fixture = Fixture::new();
    fixture.ok(&[
        "run",
        "--fg",
        "-n",
        "stdin",
        "-m",
        "2",
        "--",
        "/bin/sh",
        "-c",
        "if read -r line; then exit 9; else echo stdin-closed; fi",
    ]);
    assert_eq!(
        fixture
            .log("stdin")
            .lines()
            .filter(|line| *line == "stdin-closed")
            .count(),
        2
    );
}

#[test]
fn gate_skip_then_prompt_and_postrun_share_one_attempt() {
    let fixture = Fixture::new();
    fixture.script(
        "gate.sh",
        "#!/bin/sh\nn=$(cat gate-count 2>/dev/null || printf 0)\nn=$((n + 1))\nprintf '%s' \"$n\" > gate-count\nif [ \"$n\" -lt 3 ]; then exit 1; fi\nprintf 'gate-warning\\n' >&2\nprintf 'issue-42\\n'\n",
    );
    fixture.script(
        "postrun.sh",
        "#!/bin/sh\nset -eu\n[ \"$LOOPSIE_GATE_OUTPUT\" = issue-42 ]\n[ \"$LOOPSIE_EXIT_CODE\" = 7 ]\n[ \"$LOOPSIE_RUN_OUTPUT_TRUNCATED\" = 0 ]\ngrep -Fq 'main-result' \"$LOOPSIE_RUN_OUTPUT_FILE\"\nprintf 'postrun-ok' > postrun-receipt\n",
    );
    fs::write(
        fixture.root.join("prompt.md"),
        "Work on ${LOOPSIE_GATE_OUTPUT}.\n",
    )
    .unwrap();
    let result = fixture.run(&[
        "run",
        "--fg",
        "-n",
        "gated",
        "-m",
        "1",
        "--every",
        "10ms",
        "--gate",
        "./gate.sh",
        "--prompt-file",
        "prompt.md",
        "--postrun",
        "./postrun.sh",
        "--",
        "/bin/sh",
        "-c",
        "cat > received-prompt; printf 'main-result\\n'; exit 7",
    ]);
    assert_eq!(result.status.code(), Some(7));
    fixture.assert_finished("gated", 1, 7);
    assert_eq!(
        fs::read_to_string(fixture.root.join("gate-count")).unwrap(),
        "3"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("received-prompt")).unwrap(),
        "Work on issue-42.\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("postrun-receipt")).unwrap(),
        "postrun-ok"
    );
    assert!(!fixture.state.join("gated.run-output").exists());
    assert!(fixture.log("gated").contains("gate-warning"));
    assert_eq!(
        fixture.log("gated").matches("gate skipped command").count(),
        2
    );
}

#[test]
fn postrun_failure_is_reported_without_replaying_the_command() {
    let fixture = Fixture::new();
    fixture.script("postrun.sh", "#!/bin/sh\nexit 9\n");
    let result = fixture.run(&[
        "run",
        "--fg",
        "-n",
        "post-fail",
        "-m",
        "1",
        "--postrun",
        "./postrun.sh",
        "--",
        "/bin/sh",
        "-c",
        "printf x >> runs",
    ]);
    assert_eq!(result.status.code(), Some(9));
    fixture.assert_finished("post-fail", 1, 9);
    assert_eq!(fs::read_to_string(fixture.root.join("runs")).unwrap(), "x");
    assert!(fixture.log("post-fail").contains("postrun exited 9"));
}

#[test]
fn prompt_file_is_read_again_for_each_command() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("prompt.md"), "first\n").unwrap();
    fixture.ok(&[
        "run",
        "--fg",
        "-n",
        "reread",
        "-m",
        "2",
        "--prompt-file",
        "prompt.md",
        "--",
        "/bin/sh",
        "-c",
        "cat; if [ \"$LOOPSIE_ITERATION\" = 1 ]; then printf 'second\\n' > prompt.md; fi",
    ]);
    fixture.assert_finished("reread", 2, 0);
    let log = fixture.log("reread");
    assert!(log.contains("\nfirst\n"), "{log}");
    assert!(log.contains("\nsecond\n"), "{log}");
}

#[test]
fn background_worker_receives_gate_prompt_and_postrun_options() {
    let fixture = Fixture::new();
    fixture.script("gate.sh", "#!/bin/sh\nprintf 'job-91\\n'\n");
    fixture.script(
        "postrun.sh",
        "#!/bin/sh\nset -eu\n[ \"$LOOPSIE_GATE_OUTPUT\" = job-91 ]\ngrep -Fq 'handled:job-91' \"$LOOPSIE_RUN_OUTPUT_FILE\"\necho done > receipt\n",
    );
    fs::write(
        fixture.root.join("prompt.md"),
        "handled:${LOOPSIE_GATE_OUTPUT}\n",
    )
    .unwrap();
    fixture.ok(&[
        "run",
        "-n",
        "background-hooks",
        "-m",
        "1",
        "--gate",
        "./gate.sh",
        "--prompt-file",
        "prompt.md",
        "--postrun",
        "./postrun.sh",
        "--",
        "/bin/cat",
    ]);
    fixture.assert_finished("background-hooks", 1, 0);
    assert_eq!(
        fs::read_to_string(fixture.root.join("receipt"))
            .unwrap()
            .trim(),
        "done"
    );
}

#[test]
fn postrun_receives_bounded_output_tail() {
    let fixture = Fixture::new();
    fixture.script(
        "postrun.sh",
        "#!/bin/sh\nset -eu\n[ \"$LOOPSIE_RUN_OUTPUT_TRUNCATED\" = 1 ]\n[ \"$(wc -c < \"$LOOPSIE_RUN_OUTPUT_FILE\")\" -eq 65536 ]\ngrep -Fq FINAL_TOKEN \"$LOOPSIE_RUN_OUTPUT_FILE\"\n",
    );
    fixture.ok(&[
        "run",
        "--fg",
        "-n",
        "tail",
        "-m",
        "1",
        "--postrun",
        "./postrun.sh",
        "--",
        "/bin/sh",
        "-c",
        "yes x | head -c 70000; printf 'FINAL_TOKEN\\n'",
    ]);
    fixture.assert_finished("tail", 1, 0);
}

#[test]
fn timed_out_gate_is_cleaned_up_and_does_not_launch_the_command() {
    let fixture = Fixture::new();
    fixture.script("gate.sh", "#!/bin/sh\necho $$ > pid-gate\nsleep 30\n");
    fixture.ok(&[
        "run",
        "-n",
        "gate-timeout",
        "--every",
        "1s",
        "--gate",
        "./gate.sh",
        "--hook-timeout",
        "100ms",
        "--grace",
        "50ms",
        "--",
        "/bin/sh",
        "-c",
        "echo launched > launched",
    ]);
    wait_until(Duration::from_secs(5), || {
        fixture
            .log("gate-timeout")
            .contains("gate failed with exit 124")
    });
    fixture.ok(&["kill", "gate-timeout"]);
    fixture.wait_phase("gate-timeout", "stopped");
    assert_eq!(fixture.status("gate-timeout").unwrap()[2], "0");
    assert!(!fixture.root.join("launched").exists());
    wait_until(Duration::from_secs(5), || {
        fs::read_to_string(fixture.root.join("pid-gate"))
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
            .is_none_or(|pid| !process_alive(pid))
    });
}

#[test]
fn finite_loops_do_not_sleep_after_the_final_iteration() {
    let fixture = Fixture::new();
    let result = output(
        fixture.command(&[
            "run",
            "--fg",
            "-n",
            "final",
            "-m",
            "1",
            "--sleep",
            "30s",
            "--",
            "/bin/echo",
            "done",
        ]),
        Duration::from_secs(3),
    );
    assert_success(&result);
    fixture.assert_finished("final", 1, 0);
}

#[test]
fn fixed_intervals_space_starts_without_overlapping_commands() {
    let fixture = Fixture::new();
    fixture.ok(&[
        "run", "--fg", "-n", "interval", "-m", "3", "--every", "200ms", "--", "/bin/sh", "-c",
        "mkdir active || exit 9; echo started > \"started-$LOOPSIE_ITERATION\"; sleep 0.08; rmdir active; echo \"finished:$LOOPSIE_ITERATION\"",
    ]);
    let times: Vec<_> = (1..=3)
        .map(|iteration| {
            fs::metadata(fixture.root.join(format!("started-{iteration}")))
                .unwrap()
                .modified()
                .unwrap()
        })
        .collect();
    for pair in times.windows(2) {
        assert!(pair[1].duration_since(pair[0]).unwrap() >= Duration::from_millis(150));
    }
    for iteration in 1..=3 {
        assert!(
            fixture
                .log("interval")
                .contains(&format!("finished:{iteration}"))
        );
    }
    fixture.assert_finished("interval", 3, 0);
}

fn supervisor_signal_cleans_up(signal: i32) {
    let fixture = Fixture::new();
    let supervisor = ChildGuard::spawn(fixture.command(&[
        "run",
        "--fg",
        "-n",
        "signalled",
        "--timeout",
        "0",
        "--grace",
        "50ms",
        "--",
        "/bin/sh",
        "-c",
        "trap '' TERM; echo $$ > pid-command; sleep 30 & echo $! > pid-grandchild; wait",
    ]));
    wait_until(Duration::from_secs(5), || {
        fixture.recorded_pids().len() == 2
    });
    let supervisor_pid = supervisor.child.as_ref().unwrap().id();
    assert_eq!(unsafe { libc::kill(supervisor_pid as i32, signal) }, 0);
    let result = supervisor.output(Duration::from_secs(3));
    assert_eq!(result.status.code(), Some(128 + signal));
    fixture.assert_recorded_children_dead();
    fixture.wait_phase("signalled", "stopped");
}

#[test]
fn sigterm_to_the_supervisor_cleans_up_its_children() {
    supervisor_signal_cleans_up(libc::SIGTERM);
}

#[test]
fn sigint_to_the_supervisor_cleans_up_its_children() {
    supervisor_signal_cleans_up(libc::SIGINT);
}

#[test]
fn following_logs_continues_after_rotation() {
    let fixture = Fixture::new();
    fixture.ok(&[
        "run", "-n", "follow", "-m", "2", "--log-bytes", "4096", "--", "/bin/sh", "-c",
        "if [ \"$LOOPSIE_ITERATION\" = 1 ]; then echo follow-first; while [ ! -e release ]; do sleep 0.01; done; else i=0; while [ \"$i\" -lt 128 ]; do printf '%064d\\n' 0; i=$((i+1)); done; echo follow-last; fi",
    ]);
    wait_until(Duration::from_secs(5), || {
        fs::read_to_string(fixture.state.join("follow.log"))
            .is_ok_and(|text| text.contains("follow-first"))
    });
    let follower = ChildGuard::spawn(fixture.command(&["logs", "-f", "follow"]));
    wait_until(Duration::from_secs(5), || {
        fs::read_to_string(follower.capture.join("stdout"))
            .is_ok_and(|text| text.contains("follow-first"))
    });
    fs::write(fixture.root.join("release"), b"go").unwrap();
    fixture.wait_phase("follow", "stopped");
    wait_until(Duration::from_secs(5), || {
        fs::read_to_string(follower.capture.join("stdout"))
            .is_ok_and(|text| text.contains("follow-last"))
    });
    let snapshot = fixture.ok(&["logs", "follow"]);
    assert!(String::from_utf8_lossy(&snapshot.stdout).contains("follow-last"));
}

#[test]
fn fatal_log_rotation_errors_stop_and_reap_the_active_process_group() {
    let fixture = Fixture::new();
    fixture.ok(&[
        "run", "-n", "io-error", "--grace", "50ms", "--log-bytes", "4096", "--",
        "/bin/sh", "-c",
        "trap '' TERM; echo $$ > pid-writer; sleep 30 & echo $! > pid-helper; while [ ! -e flood ]; do sleep 0.01; done; while :; do printf '%064d\\n' 0; done",
    ]);
    wait_until(Duration::from_secs(5), || {
        fixture.recorded_pids().len() == 2
    });
    // A directory cannot be replaced by the file rename used for rotation.
    // Install it only after startup, while the command is awaiting the trigger.
    fs::create_dir(fixture.state.join("io-error.log.1")).unwrap();
    fs::write(fixture.root.join("flood"), b"go").unwrap();
    fixture.assert_finished("io-error", 1, 1);
    fixture.assert_recorded_children_dead();
}

#[test]
fn continuous_output_cannot_starve_timeouts_or_exceed_the_log_bound() {
    let fixture = Fixture::new();
    let result = output(
        fixture.command(&[
            "run",
            "--fg",
            "-n",
            "flood",
            "-m",
            "1",
            "--timeout",
            "100ms",
            "--grace",
            "50ms",
            "--log-bytes",
            "4096",
            "--",
            "/usr/bin/yes",
            "flood",
        ]),
        Duration::from_secs(3),
    );
    assert_eq!(result.status.code(), Some(124));
    fixture.assert_finished("flood", 1, 124);
    for filename in ["flood.log", "flood.log.1"] {
        assert!(fs::metadata(fixture.state.join(filename)).unwrap().len() <= 4096);
    }
}

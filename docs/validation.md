# Validation

Local validation on 2026-09-28: macOS 26.7, Apple M4 Pro, arm64,
Rust 1.98.1, Codex CLI 0.158.0, Claude Code 2.1.284.

## Automated checks

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

All checks passed: 3 unit tests and 23 integration tests. The integration tests
run real shell, Git, printf, sleep, and yes processes. They cover:

- Exact arguments, including empty strings and non-UTF-8 bytes; inherited cwd and
  environment; closed stdin; command aliases.
- Successive failures followed by recovery, spawn failures, and final exit codes.
- Timeouts and forced cleanup of commands and grandchildren that ignore TERM.
- Cleanup after a successful parent exits, and on supervisor SIGINT/SIGTERM.
- Independent background loops and concurrent attempts to claim the same name.
- Interruptible retry waits, scheduled starts, and no sleep after the final attempt.
- Log rotation, following across rotation, output floods, and cleanup after a
  fatal log write/rotation failure.
- Invalid options and stale state that must not signal an unrelated process.

The tests use isolated state directories and clean up their subprocesses. Live
agent calls are kept outside the default test suite because they require an
authenticated account and can consume paid usage.

The same formatting, Clippy, and all 26 tests also passed on Linux arm64 in a
temporary `rust:1.98.1-slim` Docker container. Git and the formatting/linting
components were installed inside that container; the source checkout was mounted
read-only and copied into its temporary directory. The container was removed
after execution. GitHub Actions is configured to repeat checks on Linux and macOS.

## Real agent CLI loops

Two invocations of each command below ran successfully through the release
binary. Each loop recorded two completed attempts, zero consecutive failures,
and final exit code 0. Both loops ran at the same time in an empty temporary
working directory, using a separate `LOOPSIE_DIR`.

```sh
loopsie run --fg --name codex-live --max 2 --sleep 100ms \
  --timeout 120s --grace 2s -- \
  codex exec --ignore-user-config --ephemeral --sandbox read-only \
  --skip-git-repo-check \
  "Reply exactly LOOPSIE_CODEX_OK. Do not use tools or edit files."

loopsie run --fg --name claude-live --max 2 --sleep 100ms \
  --timeout 120s --grace 2s -- \
  claude --safe-mode --strict-mcp-config --tools '' \
  --no-session-persistence --permission-mode dontAsk -p \
  "Reply exactly LOOPSIE_CLAUDE_OK. Do not use tools or edit files."
```

Both CLIs returned the requested markers on both invocations. These checks
exercise authenticated noninteractive inference and process supervision; the
prompts do not modify a repository. Codex's batch invocation follows its
[official noninteractive documentation](https://learn.chatgpt.com/docs/non-interactive-mode).

## Release measurements

One local smoke measurement, with an isolated state directory:

```sh
/usr/bin/time -lp target/release/loopsie run \
  --fg --name speed --max 1000 -- /usr/bin/true
```

| Measurement | Result |
| --- | ---: |
| Stripped release binary | 477,968 bytes (467 KiB) |
| 1,000 command invocations | 1.14 s wall time |
| User / system CPU time | 0.35 s / 0.55 s |
| Maximum resident memory | 2,228,224 bytes (2.13 MiB) |
| Idle supervisor resident memory | 1,664 KiB |
| Idle CPU time over two samples 2 s apart | 0:00.00 → 0:00.00 |

The command invocation measurement includes process creation, logging, state
updates, and cleanup. It is not a statistical benchmark or a claim about other
machines. The idle samples were taken while waiting for a one-hour sleep.

A separate test launched **32 background instances concurrently**, each running
`/bin/echo CONCURRENT_OK` every 100 ms. All reached at least three attempts with
zero failures. Each had at least two verified output lines. After `kill --all`,
all 34 test instance locks (including speed and idle tests) could be acquired,
all sockets were gone, and none of the recorded supervisors remained alive.

## Rust research and design

- [Rust 1.98.1 release](https://blog.rust-lang.org/2026/09/03/Rust-1.98.1/): current
  stable release when researched; fixes a vtable miscompilation in 1.98.0.
- [Rust 2024 edition](https://doc.rust-lang.org/edition-guide/rust-2024/index.html):
  current stable edition, selected in Cargo.toml.
- [Unix CommandExt](https://doc.rust-lang.org/std/os/unix/process/trait.CommandExt.html):
  stable `process_group(0)` isolates each command's process group and allows the
  standard library to use efficient process spawning.
- [Child lifecycle](https://doc.rust-lang.org/std/process/struct.Child.html):
  dropping a child does not kill or reap it. Loopsie owns cleanup explicitly and
  observes exits with `waitid(WNOWAIT)` before the final group signal, preserving
  PID ownership until cleanup is complete.
- [Cargo release profiles](https://doc.rust-lang.org/cargo/reference/profiles.html):
  default speed optimization, thin LTO, one codegen unit, and stripped symbols.
  Panic unwinding remains enabled so cleanup guards can run.

The implementation uses standard-library process, file-lock, and Unix-socket
APIs, with `libc` for the remaining signal, poll, waitid, and setsid calls.
There is no async runtime, CLI framework, serialization framework, or database.
See the README for limits around reboot, SIGKILL, and escaped process groups.

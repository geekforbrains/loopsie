# loopsie

Run a command repeatedly. Keep it running when the command fails.

A small native Rust binary for macOS and Linux. Each loop has its own process,
name, logs, and controls. There is no central daemon or built-in instance limit.
One dependency (`libc`), one supervisor thread per loop, bounded output buffers,
and event-driven waits that sleep until there is work to do.

## Install

Install [Rust](https://rustup.rs/), then build from this checkout:

```sh
cargo install --path . --locked
loopsie --version
```

This installs into `~/.cargo/bin`; make sure it is on `PATH`. To install into
`~/.local/bin` instead, use `cargo install --path . --locked --root ~/.local`.
The project pins Rust **1.98.1**, edition **2024**.

## Quick start

```sh
# Run forever, with a five-minute pause after each invocation
loopsie run --name review --sleep 5m -- \
  codex exec "Review this repository and report the next useful improvement."

# A separate Claude loop in this working directory
loopsie run --name claude-review --sleep 5m --timeout 30m -- \
  claude -p "Review this repository and report the next useful improvement."

# Any executable works
loopsie run --name status --every 30s -- git status --short

loopsie ls
loopsie logs review
loopsie logs -f review
loopsie kill review
loopsie kill --all
```

`run` starts in the background and acknowledges startup only after the instance
is ready. It inherits your current directory and environment. Arguments after
`--` are passed directly to the executable, including spaces and empty arguments.
Use `sh -c '...'` explicitly for pipes, shell builtins, or redirection.

Commands receive closed stdin. Use `codex exec`, `claude -p`, or equivalent batch
modes; interactive terminal sessions are not supported. Loopsie keeps the agent's
existing permissions and authentication. Each invocation starts a fresh command;
session continuation is controlled by the CLI arguments you supply.

## Recovery and limits

- Nonzero exits, signals, spawn failures, and timeouts trigger another attempt.
  Failure delays start at **1 second**, double up to **60 seconds**, and reset
  after a successful invocation.
- Every invocation has a **1-hour timeout** by default. Set `--timeout 30m` to
  change it, or `--timeout 0` to allow an invocation to run indefinitely.
- Stop and timeout requests send `SIGTERM` to the command's process group,
  then `SIGKILL` after **5 seconds**. `--grace` changes this upper bound.
  Once the leader exits and output closes, remaining group members are killed
  immediately. Cleanup completes before the next invocation starts.
- Logs rotate while the command runs. Each name retains a current log and one
  archive, **5 MiB each** by default. Supervisor memory does not grow with output.
- Instance names are protected by kernel file locks. Control uses a Unix socket;
  stale PID records cannot signal unrelated processes. Other names run independently.
- `SIGINT`, `SIGTERM`, and `SIGHUP` stop the supervisor and clean up its command.
  Sleep and retry waits can be interrupted immediately.

These protections cover command failures while Loopsie is running. Power loss,
reboot, `SIGKILL`, and a killed supervisor require an external service manager to
restart it. For that setup, run `loopsie run --fg ...` under launchd or systemd.
Descendants that deliberately create another process group/session can escape
group cleanup. OS resource limits still apply, and instances sharing a working
directory can interfere through the files their commands edit.

## Options

```text
loopsie run [OPTIONS] -- COMMAND [ARGS...]

  -n, --name NAME          Unique name; generated if omitted
  -e, --every DURATION     Minimum interval between invocation starts
  -s, --sleep DURATION     Delay after completion (default: 0)
  -m, --max N              Stop after N attempts (default: 0 = forever)
      --alias NAME         Prepend a saved command
      --fg                 Run in foreground; output still goes to logs
      --timeout DURATION   Invocation timeout (default: 1h; 0 disables)
      --grace DURATION     Termination grace period (default: 5s)
      --backoff DURATION   Initial failure retry delay (default: 1s)
      --max-backoff DUR    Maximum retry delay (default: 1m)
      --log-bytes N        Bytes per log file (default: 5242880; minimum: 4096)
```

Durations use whole numbers with `ms`, `s`, `m`, or `h`: `100ms`, `30s`, `1h30m`.
A bare number means seconds. Durations are limited to 365 days.
`--every` and `--sleep` are mutually exclusive. Invocations never overlap within
one loop, and a slow invocation does not cause catch-up runs. After failure,
the next invocation waits for both the schedule and retry delay.

With no schedule, successful commands restart immediately. Use `--sleep` or
`--every` when the command uses a paid API or needs a pause. `--max` counts all
attempts, including failures, and never adds a delay after the last one.
The command sees `LOOPSIE_NAME` and `LOOPSIE_ITERATION` (starting at 1).

Foreground finite loops return the final command's exit code: `124` for a timeout,
`127` for a spawn failure, or `128 + signal` for a signal. A control-socket stop
returns `0`; a direct signal to the supervisor returns `128 + signal`.
Background `run` returns startup status; use `ls` and logs for command results.
`kill` acknowledges the stop request; `ls` shows `stopped` after cleanup finishes.

## Aliases

```sh
loopsie alias set reviewer -- codex exec
loopsie run --name review --sleep 5m --alias reviewer -- "Review the latest changes."
loopsie alias ls
loopsie alias show reviewer
loopsie alias rm reviewer
```

Aliases store exact argument boundaries. They do not evaluate shell syntax.
Names use 1–48 ASCII letters, digits, underscores, or hyphens, with no leading
hyphen.

## State and logs

State defaults to `~/.loopsie`. Set `LOOPSIE_DIR` to use another directory, and
use the same value for management commands. Keep the directory path short enough
for a Unix socket (on macOS, the full socket path must be under 104 bytes).

Each name has a `.lock`, `.sock`, `.status`, `.log`, and optional `.log.1`.
Stopped status and logs remain available. `ls` shows the current phase, supervisor
PID, attempt count, consecutive failures, and last exit. An unlocked instance
without a clean shutdown is shown as `stale`. Lock files intentionally remain on
disk; never remove a live loop's files. To reuse a stopped name, just run it again.

`logs NAME` prints both retained generations. `logs -f NAME` follows the current
log across rotation until interrupted. A slow follower can miss generations
already discarded by rotation. Logs include command output and lifecycle entries
with Unix timestamps. New state directories use mode `0700`; new files use `0600`.

Upgrading from 0.1.x: stop existing loops with the old executable first. The new
version keeps the command vocabulary and log paths, but uses a new state and
alias format. Recreate aliases with `loopsie alias set`. Old `.pid` files block
reuse of that name until you remove them after stopping the old loop. Legacy
`aliases.json` and `.meta.json` files are no longer read.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

Integration tests execute real shell and Git commands in isolated temporary
directories. CI runs them on macOS and Linux. See [validation](docs/validation.md)
for the local agent CLI tests, measurements, and Rust design sources.

MIT licensed.

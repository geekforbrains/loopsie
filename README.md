<div align="center">

# loopsie

**Make a loop, grab a beer** 🍻

[![Rust](https://img.shields.io/badge/rust-1.98.1-orange.svg)](https://www.rust-lang.org/)
[![Dependencies](https://img.shields.io/badge/dependencies-just_libc-brightgreen.svg)](Cargo.toml)

</div>

---

You've got a command. You want it to run over and over. Maybe with a delay, maybe not. You don't want to write a bash `while` loop like an animal. You want to name it, background it, and forget about it.

That's it. That's the tool.

One small binary for macOS and Linux, one dependency, and some actual seatbelts for when your command shits itself.

## Install

Grab [Rust](https://rustup.rs/), then run this from the checkout:

```sh
cargo install --path . --locked
loopsie --version
```

The checkout pins Rust **1.98.1**, edition **2024**. Put `~/.cargo/bin` on your `PATH`. Prefer `~/.local/bin`? Add `--root ~/.local` to the install command.

## Quick start

```sh
# Run a command every 5 minutes
loopsie run --every 5m -- curl -fsS https://example.com/health

# Give it a 30s breather between runs
loopsie run --sleep 30s -- ./check-things.sh

# Run successful commands in a tight loop and say fuck it (yolo)
loopsie run -- echo "are we there yet"
```

Loops run in the background by default. Each one gets its own process. Start as many as your machine can stomach.

## The *real* reason you're here 🤖

```sh
loopsie run --sleep 5m --name codemonkey -- \
  claude -p "Check the repo for open TODOs and fix one. When none are left, run: loopsie kill codemonkey"

# Codex can have a job too
loopsie run --sleep 5m --name codexmonkey --timeout 30m -- \
  codex exec "Review the latest changes and report the next useful improvement."
```

Set it. Forget it. Go touch grass, or your... nvm.

Use `claude -p`, `codex exec`, or your CLI's equivalent batch mode. Commands get closed stdin, so an interactive agent waiting for you to press Enter is going to have a bad time. Your agents keep their usual permissions and authentication.

Each run starts a fresh command in the directory where you launched the loop, with your environment. Want to resume an agent session? Pass that CLI's resume options. Running several agents in the same repo is still several agents editing the same repo. Choose your chaos.

## When shit goes sideways

- **Command crashed?** Retry. Delays start at **1s**, double up to **60s**, and reset after a successful run. Missing executables and failed starts get retried too.
- **Command stuck?** Each attempt gets **1 hour** by default. Change it with `--timeout 30m`, or use `--timeout 0` if you really do mean forever.
- **Command won't leave?** Stop and timeout requests send TERM to its process group, then KILL after **5s**. Set `--grace` to change that upper bound. Once the command exits and its output closes, leftover group members get killed immediately. Cleanup happens before the next attempt.
- **Command won't shut up?** Logs rotate while it runs. Two files, **5 MiB each**, by default. Your disk doesn't need the complete memoirs of `echo`.
- **Two loops want the same name?** One wins. File locks prevent duplicate owners, and control sockets mean a stale PID can't get some unrelated process murdered.

This keeps failed commands from taking the loop down. Reboots, power cuts, or killing Loopsie itself still need an external service manager to restart it. Use `--fg` under launchd or systemd for that. Processes that deliberately leave the command's process group can escape cleanup.

## Aliases (for the truly lazy)

Tired of typing the same command prefix every time? Same.

```sh
# Save it once
loopsie alias set claude -- claude -p

# Use it forever — everything after -- gets appended
loopsie run --sleep 5m --alias claude -- "Review the latest changes"

loopsie alias ls
loopsie alias show claude
loopsie alias rm claude
```

Aliases preserve argument boundaries. No shell magic. If you want pipes, redirects, or shell builtins, say so: `loopsie run --sleep 30s -- sh -c 'git status --short | head'`.

## Managing your loops

```sh
loopsie ls                 # what's running? what died?
loopsie logs codemonkey    # what did it do?
loopsie logs -f codemonkey # what is it doing right now?
loopsie kill codemonkey    # ok that's enough
loopsie kill --all         # everybody out
```

`kill` acknowledges the stop request; `ls` shows `stopped` when cleanup finishes. Foreground loops also clean up on Ctrl-C, TERM, or HUP. Sleep and retry waits are interruptible. You don't have to sit through the rest of a five-minute nap.

## Full CLI

```text
loopsie run [OPTIONS] -- COMMAND [ARGS...]

  -n, --name NAME          Name this loop (generated if omitted)
  -e, --every DURATION     Minimum interval between starts
  -s, --sleep DURATION     Delay after completion (default: 0)
  -m, --max N              Stop after N attempts (default: 0 = forever)
      --alias NAME         Use a saved alias as command prefix
      --fg                 Stay in foreground; output still goes to logs
      --timeout DURATION   Limit each attempt (default: 1h; 0 disables)
      --grace DURATION     Time before force kill (default: 5s)
      --backoff DURATION   Initial retry delay (default: 1s)
      --max-backoff DUR    Maximum retry delay (default: 1m)
      --log-bytes N        Bytes per log file (default: 5242880; minimum: 4096)

loopsie ls
loopsie logs [-f|--follow] NAME
loopsie kill NAME | --all
loopsie alias set NAME -- COMMAND [ARGS...]
loopsie alias ls | show NAME | rm NAME
```

Durations: `100ms`, `30s`, `5m`, `2h`, `1h30m` — you get it. Whole numbers only; bare numbers mean seconds. Maximum duration: 365 days. Names: 1–48 ASCII letters, digits, underscores, or hyphens, with no leading hyphen.

Pick `--every` or `--sleep`, not both. One loop never overlaps its own commands or tries to catch up on missed runs. After a failure, both the schedule and retry delay apply. Without a schedule, successful commands restart immediately. Maybe give the paid API a breather.

`--max` counts failed attempts too, and there's no pointless sleep after the last one. Commands receive `LOOPSIE_NAME` and `LOOPSIE_ITERATION`, starting at 1.

Foreground finite loops return the last command's exit code: `124` for timeout, `127` for failure to start, or `128 + signal` for signal termination. `loopsie kill` makes the supervisor exit `0`; directly signalling it returns `128 + signal`. Background startup returns once the loop is ready; check `ls` and logs for what happens next.

## Design philosophy

- **Small and fast.** A stripped native binary. Rust 2024, one dependency (`libc`), one supervisor thread per loop. No async runtime, CLI framework, database, or interpretive dance.
- **No central daemon.** Each loop is its own background process. Nothing running = nothing running. No built-in limit on instances; your OS gets the final say.
- **Sleep means sleep.** Event-driven waits, bounded output buffers, and no growing pile of command output in memory.
- **State in `~/.loopsie/`.** Locks, sockets, plain text status, and rotating logs. Set `LOOPSIE_DIR` to put them somewhere else, and use the same value for management commands.

Stopped status and logs stick around. `ls` shows the phase, PID, attempts, consecutive failures, and last exit. Reuse a stopped name by running it again. Leave live state files alone; the lock files stay on disk on purpose.

`logs` prints both retained files. `logs -f` follows the current file across rotation until you interrupt it; a slow follower can miss output that's already rotated away. New directories use mode `0700`, files `0600`. Keep custom state paths short: macOS needs the full socket path under 104 bytes.

### Coming from 0.1.x?

Stop your old loops with the old executable first. The commands and log paths are familiar, but state and aliases have a new format. Recreate aliases with `loopsie alias set`. Remove old `.pid` files after stopping those loops so their names can be reused. Old `aliases.json` and `.meta.json` files are no longer read.

## Contributing

Submit PRs so I can ignore them. Bonus points if you're a huge douche about it.

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

The tests run real commands, break things on purpose, and check that the children get cleaned up. CI is configured for macOS and Linux. [Validation notes](docs/validation.md) cover the real Codex and Claude loops, 32 concurrent instances, and local speed and memory measurements.

## License

[MIT](LICENSE) — do whatever you want.

...Why the fuck are you still reading this.

<div align="center">

# loopsie

**Make a loop, grab a beer** 🍻

[![Python](https://img.shields.io/badge/python-3.8+-blue.svg)](https://python.org)
[![Dependencies](https://img.shields.io/badge/dependencies-zero-brightgreen.svg)](https://docs.python.org/3/library/index.html)

</div>

---

You've got a command. You want it to run over and over. Maybe with a delay, maybe not. You don't want to write a bash `while` loop like an animal. You want to name it, background it, and forget about it.

That's it. That's the tool.

## Install

```sh
pip install loopsie
```

## Quick start

```sh
# Run a command every 5 minutes
loopsie run --every 5m -- curl -s https://example.com/health

# Run a command in a loop with a 30s breather between runs
loopsie run --sleep 30s -- ./check-things.sh

# Run a command in a tight loop and say fuck it (yolo)
loopsie run -- echo "are we there yet"
```

## The *real* reason you're here 🤖

```sh
loopsie run --sleep 5m --name codemonkey -- \
  claude -p "Check the repo for open TODOs and fix one. When none are left, run: loopsie kill codemonkey"
```

Set it. Forget it. Go touch grass, or your... nvm.

## Aliases (for the truly lazy)

Tired of typing `claude --model opus --dangerously-skip-permissions` every time? Same.

Make an alias like a dandy little bitch:

```sh
# Save it once
loopsie alias set claude -- claude --model opus --dangerously-skip-permissions -p

# Use it forever — everything after -- gets appended
loopsie run --sleep 5m --alias claude -- "Review the latest PR and leave comments"
```

## Managing your loops

```sh
loopsie ls                  # what's running?
loopsie logs codemonkey     # what did it do?
loopsie logs -f codemonkey  # what is it doing right now?
loopsie kill codemonkey     # ok that's enough
loopsie kill --all          # everybody out
```

## Full CLI

```
loopsie run [OPTIONS] -- COMMAND [ARGS...]

  -n, --name NAME       Name this loop (auto-generated if omitted)
  -e, --every DURATION   Fixed interval between starts (e.g. 5m, 1h, 30s)
  -s, --sleep DURATION   Delay between completions (e.g. 30s, 1m)
  -m, --max N           Stop after N iterations
  --alias NAME          Use a saved alias as command prefix
  --fg                  Run in foreground (don't daemonize)

loopsie ls              List running loops
loopsie logs NAME       View loop output (-f to follow)
loopsie kill NAME       Stop a loop (--all to stop everything)

loopsie alias set NAME -- COMMAND [ARGS...]
loopsie alias ls
loopsie alias show NAME
loopsie alias rm NAME
```

Durations: `30s`, `5m`, `2h`, `1h30m` — you get it.

## Design philosophy

- **Zero dependencies.** Python stdlib only. No `click`, no `rich`, no `yaml`. Just `argparse` and `os.fork()` like nature intended.
- **Single file.** The entire tool is one Python file. Read it in 5 minutes.
- **No daemon.** Each loop is its own background process. Nothing running = nothing running.
- **State in `~/.loopsie/`.** PID files, JSON metadata, plain text logs. `rm -rf` it if things get weird.

## Contributing

Submit PRs so I can ignore them. Bonus points if you're a huge douche about it.

## License

MIT — do whatever you want.

...Why the fuck are you still reading this.

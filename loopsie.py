#!/usr/bin/env python3
"""loopsie - run commands in loops."""

import argparse
import json
import os
import random
import signal
import string
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

LOOPSIE_DIR = Path(os.environ.get("LOOPSIE_DIR", str(Path.home() / ".loopsie")))
ALIASES_FILE = LOOPSIE_DIR / "aliases.json"


def ensure_dir():
    LOOPSIE_DIR.mkdir(parents=True, exist_ok=True)


# --- Duration parsing ---


def parse_duration(s):
    """Parse duration string like '5m', '1h30m', '30s' into seconds."""
    if not s:
        return 0
    total = 0
    current = ""
    for c in s:
        if c.isdigit():
            current += c
        elif c == "s":
            total += int(current)
            current = ""
        elif c == "m":
            total += int(current) * 60
            current = ""
        elif c == "h":
            total += int(current) * 3600
            current = ""
        else:
            raise ValueError(f"Invalid duration: {s}")
    if current:
        total += int(current)
    return total


# --- Name generation ---


def generate_name():
    """Generate a short random loop name."""
    return "loop-" + "".join(random.choices(string.hexdigits[:16], k=4))


# --- Path helpers ---


def get_meta_path(name):
    return LOOPSIE_DIR / f"{name}.meta.json"


def get_pid_path(name):
    return LOOPSIE_DIR / f"{name}.pid"


def get_log_path(name):
    return LOOPSIE_DIR / f"{name}.log"


# --- Alias management ---


def load_aliases():
    if ALIASES_FILE.exists():
        return json.loads(ALIASES_FILE.read_text())
    return {}


def save_aliases(aliases):
    ensure_dir()
    ALIASES_FILE.write_text(json.dumps(aliases, indent=2) + "\n")


def cmd_alias(args):
    if args.alias_cmd == "set":
        aliases = load_aliases()
        aliases[args.name] = args.command
        save_aliases(aliases)
        print(f"Alias '{args.name}' set: {' '.join(args.command)}")
    elif args.alias_cmd == "ls":
        aliases = load_aliases()
        if not aliases:
            print("No aliases defined.")
            return
        for name, cmd in aliases.items():
            print(f"  {name}: {' '.join(cmd)}")
    elif args.alias_cmd == "show":
        aliases = load_aliases()
        if args.name not in aliases:
            print(f"Alias '{args.name}' not found.", file=sys.stderr)
            sys.exit(1)
        print(" ".join(aliases[args.name]))
    elif args.alias_cmd == "rm":
        aliases = load_aliases()
        if args.name not in aliases:
            print(f"Alias '{args.name}' not found.", file=sys.stderr)
            sys.exit(1)
        del aliases[args.name]
        save_aliases(aliases)
        print(f"Alias '{args.name}' removed.")
    else:
        print("Usage: loopsie alias {set,ls,show,rm}", file=sys.stderr)
        sys.exit(1)


# --- State management ---


def save_meta(name, meta):
    get_meta_path(name).write_text(json.dumps(meta, indent=2) + "\n")


def load_meta(name):
    p = get_meta_path(name)
    if p.exists():
        return json.loads(p.read_text())
    return None


def is_running(name):
    pid_path = get_pid_path(name)
    if not pid_path.exists():
        return False
    try:
        pid = int(pid_path.read_text().strip())
        os.kill(pid, 0)
        return True
    except (OSError, ValueError):
        return False


def cleanup(name):
    """Remove pid and meta files for a stopped loop."""
    get_pid_path(name).unlink(missing_ok=True)
    get_meta_path(name).unlink(missing_ok=True)


def get_all_loops():
    """Get all loop names that have pid files."""
    if not LOOPSIE_DIR.exists():
        return []
    return sorted(f.stem for f in LOOPSIE_DIR.iterdir() if f.suffix == ".pid")


# --- Loop engine ---


def run_loop(name, command, every, sleep_dur, max_iter, meta):
    """Run the command in a loop. This blocks until done."""
    log_path = get_log_path(name)
    iteration = 0

    while True:
        iteration += 1
        if max_iter and iteration > max_iter:
            break

        start = time.time()
        now = datetime.now().isoformat(timespec="seconds")

        with open(log_path, "a") as log:
            log.write(f"--- [{name}] iteration {iteration} | {now} ---\n")
            log.flush()

            try:
                result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
                log.write(f"--- exit: {result.returncode} ---\n\n")
            except FileNotFoundError:
                log.write(f"--- error: command not found: {command[0]} ---\n\n")
            except Exception as e:
                log.write(f"--- error: {e} ---\n\n")

        meta["iteration"] = iteration
        meta["last_run"] = datetime.now().isoformat(timespec="seconds")
        save_meta(name, meta)

        if every:
            elapsed = time.time() - start
            wait = max(0, every - elapsed)
            if wait > 0:
                time.sleep(wait)
        elif sleep_dur:
            time.sleep(sleep_dur)


# --- Commands ---


def cmd_run(args):
    ensure_dir()

    # Resolve command
    command = []
    if args.alias:
        aliases = load_aliases()
        if args.alias not in aliases:
            print(f"Alias '{args.alias}' not found.", file=sys.stderr)
            sys.exit(1)
        command = aliases[args.alias]

    if args.command:
        command = command + args.command

    if not command:
        print("No command specified. Use -- COMMAND or --alias NAME.", file=sys.stderr)
        sys.exit(1)

    # Validate flags
    if args.every and args.sleep:
        print("Cannot use both --every and --sleep.", file=sys.stderr)
        sys.exit(1)

    every = parse_duration(args.every) if args.every else None
    sleep_dur = parse_duration(args.sleep) if args.sleep else 0
    max_iter = args.max

    name = args.name or generate_name()

    if is_running(name):
        print(f"Loop '{name}' is already running.", file=sys.stderr)
        sys.exit(1)

    meta = {
        "name": name,
        "command": command,
        "every": args.every,
        "sleep": args.sleep,
        "max": max_iter,
        "started_at": datetime.now().isoformat(timespec="seconds"),
        "iteration": 0,
    }

    if args.fg:
        get_pid_path(name).write_text(str(os.getpid()))
        save_meta(name, meta)
        if args.every:
            mode = f"every {args.every}"
        elif args.sleep:
            mode = f"sleep {args.sleep}"
        else:
            mode = "loop"
        print(f"[{name}] {' '.join(command)} ({mode})")
        try:
            run_loop(name, command, every, sleep_dur, max_iter, meta)
        except KeyboardInterrupt:
            print(f"\n[{name}] stopped.")
        finally:
            cleanup(name)
        return

    # Daemonize
    pid = os.fork()
    if pid > 0:
        # Parent: wait a beat for the child to write its pid, then report
        time.sleep(0.05)
        print(f"Started '{name}' (pid {pid}): {' '.join(command)}")
        if args.every:
            print(f"  every: {args.every}")
        if args.sleep:
            print(f"  sleep: {args.sleep}")
        if max_iter:
            print(f"  max: {max_iter}")
        return

    # Child
    os.setsid()

    devnull = os.open(os.devnull, os.O_RDWR)
    os.dup2(devnull, 0)
    os.dup2(devnull, 1)
    os.dup2(devnull, 2)
    os.close(devnull)

    get_pid_path(name).write_text(str(os.getpid()))
    save_meta(name, meta)

    def handle_signal(sig, frame):
        cleanup(name)
        sys.exit(0)

    signal.signal(signal.SIGTERM, handle_signal)
    signal.signal(signal.SIGINT, handle_signal)

    try:
        run_loop(name, command, every, sleep_dur, max_iter, meta)
    finally:
        cleanup(name)
    sys.exit(0)


def cmd_ls(args):
    loops = get_all_loops()
    if not loops:
        print("No loops running.")
        return

    # Filter to live loops, clean up stale ones
    alive = []
    for name in loops:
        if is_running(name):
            alive.append(name)
        else:
            cleanup(name)

    if not alive:
        print("No loops running.")
        return

    hdr = f"{'NAME':<16} {'STATUS':<10} {'PID':<8} "
    hdr += f"{'EVERY':<8} {'SLEEP':<8} {'ITER':<6} STARTED"
    print(hdr)

    for name in alive:
        meta = load_meta(name) or {}
        pid = get_pid_path(name).read_text().strip()
        every = meta.get("every") or "-"
        sleep_val = meta.get("sleep") or "-"
        iteration = meta.get("iteration", 0)

        started_fmt = "-"
        started = meta.get("started_at")
        if started:
            try:
                dt = datetime.fromisoformat(started)
                secs = int((datetime.now() - dt).total_seconds())
                if secs < 60:
                    started_fmt = f"{secs}s ago"
                elif secs < 3600:
                    started_fmt = f"{secs // 60}m ago"
                else:
                    h, rem = divmod(secs, 3600)
                    started_fmt = f"{h}h{rem // 60}m ago"
            except ValueError:
                started_fmt = started

        row = f"{name:<16} {'running':<10} {pid:<8} "
        row += f"{every:<8} {sleep_val:<8} {iteration:<6} {started_fmt}"
        print(row)


def cmd_logs(args):
    log_path = get_log_path(args.name)
    if not log_path.exists():
        print(f"No logs for '{args.name}'.", file=sys.stderr)
        sys.exit(1)

    if args.follow:
        with open(log_path) as f:
            print(f.read(), end="")
            try:
                while True:
                    line = f.readline()
                    if line:
                        print(line, end="")
                    else:
                        time.sleep(0.5)
            except KeyboardInterrupt:
                pass
    else:
        print(log_path.read_text(), end="")


def cmd_kill(args):
    if args.all:
        loops = get_all_loops()
        if not loops:
            print("No loops running.")
            return
        for name in loops:
            _kill_one(name)
        return

    if not args.name:
        print("Specify a loop name or --all.", file=sys.stderr)
        sys.exit(1)

    _kill_one(args.name)


def _kill_one(name):
    pid_path = get_pid_path(name)
    if not pid_path.exists():
        print(f"Loop '{name}' not found.", file=sys.stderr)
        return

    try:
        pid = int(pid_path.read_text().strip())
        os.kill(pid, signal.SIGTERM)
        print(f"Killed '{name}' (pid {pid}).")
    except (OSError, ValueError):
        print(f"Loop '{name}' was not running (stale).")

    cleanup(name)


# --- CLI ---


def main():
    parser = argparse.ArgumentParser(
        prog="loopsie", description="Run commands in loops."
    )
    sub = parser.add_subparsers(dest="cmd")

    # --- run ---
    p_run = sub.add_parser("run", help="Start a loop")
    p_run.add_argument("-n", "--name", help="Name this loop")
    p_run.add_argument("-e", "--every", help="Fixed interval between starts (e.g. 5m)")
    p_run.add_argument("-s", "--sleep", help="Delay between completions (e.g. 30s)")
    p_run.add_argument("-m", "--max", type=int, help="Stop after N iterations")
    p_run.add_argument("--alias", help="Use a saved alias as command prefix")
    p_run.add_argument("--fg", action="store_true", help="Run in foreground")
    p_run.add_argument("command", nargs="*", help="Command to run (after --)")

    # --- ls ---
    sub.add_parser("ls", help="List running loops")

    # --- logs ---
    p_logs = sub.add_parser("logs", help="View loop output")
    p_logs.add_argument("name", help="Loop name")
    p_logs.add_argument("-f", "--follow", action="store_true", help="Follow output")

    # --- kill ---
    p_kill = sub.add_parser("kill", help="Stop a loop")
    p_kill.add_argument("name", nargs="?", help="Loop name")
    p_kill.add_argument("--all", action="store_true", help="Kill all loops")

    # --- alias ---
    p_alias = sub.add_parser("alias", help="Manage command aliases")
    alias_sub = p_alias.add_subparsers(dest="alias_cmd")

    p_alias_set = alias_sub.add_parser("set", help="Create or update an alias")
    p_alias_set.add_argument("name", help="Alias name")
    p_alias_set.add_argument("command", nargs="+", help="Command (after --)")

    alias_sub.add_parser("ls", help="List aliases")

    p_alias_show = alias_sub.add_parser("show", help="Show an alias")
    p_alias_show.add_argument("name")

    p_alias_rm = alias_sub.add_parser("rm", help="Remove an alias")
    p_alias_rm.add_argument("name")

    args = parser.parse_args()

    if args.cmd == "run":
        cmd_run(args)
    elif args.cmd == "ls":
        cmd_ls(args)
    elif args.cmd == "logs":
        cmd_logs(args)
    elif args.cmd == "kill":
        cmd_kill(args)
    elif args.cmd == "alias":
        cmd_alias(args)
    else:
        parser.print_help()


if __name__ == "__main__":
    main()

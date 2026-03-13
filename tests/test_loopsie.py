"""Tests for loopsie."""

import os
import subprocess
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import loopsie as lp


@pytest.fixture
def tmp_loopsie(tmp_path, monkeypatch):
    """Redirect loopsie state to a temp directory."""
    monkeypatch.setattr(lp, "LOOPSIE_DIR", tmp_path)
    monkeypatch.setattr(lp, "ALIASES_FILE", tmp_path / "aliases.json")
    return tmp_path


# --- Duration parsing ---


class TestParseDuration:
    def test_seconds(self):
        assert lp.parse_duration("30s") == 30

    def test_minutes(self):
        assert lp.parse_duration("5m") == 300

    def test_hours(self):
        assert lp.parse_duration("2h") == 7200

    def test_combined(self):
        assert lp.parse_duration("1h30m") == 5400

    def test_bare_number(self):
        assert lp.parse_duration("60") == 60

    def test_empty(self):
        assert lp.parse_duration("") == 0

    def test_none(self):
        assert lp.parse_duration(None) == 0

    def test_invalid(self):
        with pytest.raises(ValueError):
            lp.parse_duration("5x")


# --- Name generation ---


def test_generate_name():
    name = lp.generate_name()
    assert name.startswith("loop-")
    assert len(name) == 9


# --- Aliases ---


class TestAliases:
    def test_load_empty(self, tmp_loopsie):
        assert lp.load_aliases() == {}

    def test_save_and_load(self, tmp_loopsie):
        lp.save_aliases({"foo": ["echo", "hello"]})
        assert lp.load_aliases() == {"foo": ["echo", "hello"]}

    def test_overwrite(self, tmp_loopsie):
        lp.save_aliases({"foo": ["echo", "1"]})
        lp.save_aliases({"foo": ["echo", "2"]})
        assert lp.load_aliases()["foo"] == ["echo", "2"]


# --- State helpers ---


class TestState:
    def test_is_running_no_pid(self, tmp_loopsie):
        assert not lp.is_running("nope")

    def test_is_running_stale(self, tmp_loopsie):
        lp.get_pid_path("stale").write_text("999999")
        assert not lp.is_running("stale")

    def test_cleanup(self, tmp_loopsie):
        lp.get_pid_path("x").write_text("1")
        lp.save_meta("x", {"name": "x"})
        lp.cleanup("x")
        assert not lp.get_pid_path("x").exists()
        assert not lp.get_meta_path("x").exists()

    def test_get_all_loops(self, tmp_loopsie):
        lp.get_pid_path("a").write_text("1")
        lp.get_pid_path("b").write_text("2")
        assert lp.get_all_loops() == ["a", "b"]


# --- Loop engine ---


class TestRunLoop:
    def test_runs_command_max_1(self, tmp_loopsie):
        meta = {
            "name": "t",
            "command": ["echo", "hi"],
            "every": None,
            "sleep": None,
            "max": 1,
            "started_at": "2024-01-01T00:00:00",
            "iteration": 0,
        }
        lp.run_loop("t", ["echo", "hi"], None, 0, 1, meta)

        log = lp.get_log_path("t").read_text()
        assert "iteration 1" in log
        assert "hi" in log
        assert "exit: 0" in log

    def test_runs_multiple_iterations(self, tmp_loopsie):
        meta = {
            "name": "multi",
            "command": ["echo", "ok"],
            "every": None,
            "sleep": None,
            "max": 3,
            "started_at": "2024-01-01T00:00:00",
            "iteration": 0,
        }
        lp.run_loop("multi", ["echo", "ok"], None, 0, 3, meta)

        log = lp.get_log_path("multi").read_text()
        assert "iteration 1" in log
        assert "iteration 2" in log
        assert "iteration 3" in log
        assert log.count("exit: 0") == 3

    def test_bad_command(self, tmp_loopsie):
        meta = {
            "name": "bad",
            "command": ["__nonexistent_cmd__"],
            "every": None,
            "sleep": None,
            "max": 1,
            "started_at": "2024-01-01T00:00:00",
            "iteration": 0,
        }
        lp.run_loop("bad", ["__nonexistent_cmd__"], None, 0, 1, meta)

        log = lp.get_log_path("bad").read_text()
        assert "command not found" in log


# --- CLI integration ---


class TestCLI:
    def test_run_foreground_max(self, tmp_loopsie):
        """End-to-end: run in foreground with --max."""
        cmd = [
            sys.executable,
            "-m",
            "loopsie",
            "run",
            "--fg",
            "-m",
            "2",
            "--",
            "echo",
            "yo",
        ]
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            env={**os.environ, "LOOPSIE_DIR": str(tmp_loopsie)},
            cwd=os.path.join(os.path.dirname(__file__), ".."),
            timeout=10,
        )
        assert result.returncode == 0

        # Find the log file
        logs = list(tmp_loopsie.glob("*.log"))
        assert len(logs) == 1
        content = logs[0].read_text()
        assert "yo" in content
        assert "iteration 2" in content

    def test_alias_roundtrip(self, tmp_loopsie):
        """Set an alias, list it, show it, remove it."""
        env = {**os.environ, "LOOPSIE_DIR": str(tmp_loopsie)}
        base = [sys.executable, "-m", "loopsie"]
        cwd = os.path.join(os.path.dirname(__file__), "..")

        def run(a):
            return subprocess.run(
                base + a,
                capture_output=True,
                text=True,
                env=env,
                cwd=cwd,
            )

        r = run(["alias", "set", "hi", "--", "echo", "hello"])
        assert r.returncode == 0
        assert "set" in r.stdout

        r = run(["alias", "ls"])
        assert "hi" in r.stdout

        r = run(["alias", "show", "hi"])
        assert "echo hello" in r.stdout

        r = run(["alias", "rm", "hi"])
        assert "removed" in r.stdout

"""Exercise the shipped units with the real user manager and isolated tmux.

Launched by systemd_restart.rs. Only the agent backend is mocked; the daemon,
control client, systemd dependency propagation, and tmux scopes are real.
"""

import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import time
import uuid


def run(*args, env=None):
    result = subprocess.run(args, env=env, capture_output=True, text=True, timeout=30)
    if result.returncode:
        raise AssertionError(f"{args}: {result.stdout}\n{result.stderr}")
    return result.stdout.strip()


def ctl(*args):
    return run("systemctl", "--user", *args)


def prop(unit, name):
    return ctl("show", unit, "-p", name, "--value")


def wait_for(check, label):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError(f"Timed out waiting for {label}")


def process_identity(pid):
    path = Path(f"/proc/{pid}/stat")
    if not path.exists():
        return None
    fields = path.read_text().rsplit(")", 1)[1].split()
    return None if fields[0] == "Z" else (pid, fields[19])


def exercise(with_companion):
    repo = Path(__file__).resolve().parents[2]
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    units = runtime / "systemd/user"
    units.mkdir(parents=True, exist_ok=True)
    prefix = "slopd-restart-test-" + uuid.uuid4().hex[:12]
    daemon = prefix + ".service"
    owner = prefix + "-tmux.service"
    files = [units / daemon, units / owner]
    real_tmux = shutil.which("tmux")
    assert real_tmux, "tmux is required"

    with tempfile.TemporaryDirectory(prefix=prefix + "-", dir=runtime) as temp:
        root = Path(temp)
        socket = str(root / "tmux.sock")
        tmux = [real_tmux, "-S", socket]
        overrides = {}
        for key, directory in [("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"),
                               ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state")]:
            (root / directory).mkdir()
            overrides[key] = str(root / directory)
        # Keep the real XDG_RUNTIME_DIR so tmux can reach the user systemd bus.
        # Explicit private sockets and config/state paths isolate everything else.
        (root / "bin").mkdir()
        guard = root / "bin/tmux"
        guard.write_text(
            '#!/bin/sh\n'
            f'if [ "$1" != "-S" ] || [ "$2" != {shlex.quote(socket)} ]; then\n'
            '  echo "Rejected tmux command outside private test socket" >&2\n'
            '  exit 99\nfi\n'
            f'exec {shlex.quote(real_tmux)} "$@"\n'
        )
        guard.chmod(0o700)
        overrides["PATH"] = str(root / "bin") + os.pathsep + os.environ["PATH"]
        overrides["CLAUDE_CONFIG_DIR"] = str(root / "claude")
        env = {k: v for k, v in os.environ.items() if not k.startswith(("TMUX", "SLOP"))}
        env.update(overrides)
        rejected = subprocess.run([str(guard), "list-sessions"], capture_output=True)
        assert rejected.returncode == 99, "tmux isolation guard failed"

        config = root / "slopd.toml"
        control = str(root / "slopd.sock")
        config.write_text(
            f'config_dir = {json.dumps(str(root / "claude"))}\n'
            f'[tmux]\nsocket = {json.dumps(socket)}\nstart_server = true\n'
            f'[run]\nslopctl = {json.dumps(os.environ["SLOPCTL_TEST_BINARY"])}\n'
            f'executable = {json.dumps(os.environ["MOCK_CLAUDE_TEST_BINARY"])}\n'
            f'start_directory = {json.dumps(temp)}\n'
            '[backup]\nauto_backup = false\nauto_restore = false\n'
        )
        cli = [os.environ["SLOPCTL_TEST_BINARY"], "--config", str(config), "--socket", control]
        service = (repo / "slopd.service").read_text().replace("slopd-tmux.service", owner)
        if not with_companion:
            # Negative control: retain KillMode=process but reproduce the old
            # layout where the daemon itself starts the tmux server.
            service = service.replace(f"Requires={owner}\n", "").replace(f"After={owner}\n", "")
        command = [os.environ["SLOPD_TEST_BINARY"], "--config", str(config), "--socket", control, "-vv"]
        service = service.replace(
            "ExecStart=%h/.cargo/bin/slopd",
            "ExecStart=" + " ".join(json.dumps(arg) for arg in command),
        )
        service += f"\n[Service]\nWorkingDirectory={temp}\n"
        service += "UnsetEnvironment=TMUX TMUX_PANE\n"
        for key, value in overrides.items():
            service += "Environment=" + json.dumps(f"{key}={value}") + "\n"
        companion = (repo / "slopd-tmux.service").read_text().replace(
            "/usr/bin/tmux start-server",
            f'{json.dumps(real_tmux)} -S {json.dumps(socket)} -f /dev/null start-server',
        )
        pane_id = None

        def ready():
            assert prop(daemon, "ActiveState") == "active", "daemon exited"
            result = subprocess.run([*cli, "status"], env=env, capture_output=True, timeout=5)
            return result.returncode == 0

        def info():
            panes = json.loads(run(*cli, "ps", "--json", env=env))
            return next((p for p in panes if p["pane_id"] == pane_id), None)

        def identity():
            values = run(*tmux, "display-message", "-p", "-t", pane_id,
                         "#{pid} #{pane_pid} #{pane_dead}").split()
            assert values[2] == "0", "agent pane died"
            processes = tuple(process_identity(pid) for pid in values[:2])
            assert all(processes), "server or agent process exited"
            return processes

        def exchange_prompt(label):
            prompt = f"restart-regression-{label}"
            run(*cli, "send", "--timeout", "10", pane_id, prompt, env=env)
            transcript = Path(info()["transcript_path"])
            wait_for(lambda: f"mock response to: {prompt}" in transcript.read_text(),
                     f"response after {label}")
            wait_for(lambda: info()["state"] == "ready", f"ready after {label}")

        try:
            files[0].write_text(service)
            files[1].write_text(companion)
            ctl("daemon-reload")
            ctl("start", daemon)
            wait_for(ready, "control socket")
            pane_id = run(*cli, "run", "--no-wait", env=env)
            wait_for(lambda: (p := info()) and p.get("session_id") and p["state"] == "ready",
                     "agent session binding")
            original = identity()
            saved = {key: info()[key] for key in ["session_id", "transcript_path", "backend", "account"]}
            scope = ctl("whoami", original[1][0])
            assert scope.startswith("tmux-spawn-"), "tmux must support systemd pane scopes"
            assert prop(scope, "PartOf") == (owner if with_companion else daemon)
            exchange_prompt("startup")

            if not with_companion:
                ctl("stop", daemon)
                wait_for(lambda: process_identity(original[1][0]) is None, "original pane death")
                assert process_identity(original[0][0]) == original[0], "tmux itself should survive"
                print("Reproduced: KillMode=process kept tmux alive but its agent scope died.", flush=True)
                return

            assert prop(owner, "MainPID") == original[0][0]
            assert prop(daemon, "ConsistsOf") == ""
            previous_daemon = process_identity(prop(daemon, "MainPID"))
            for action in ["restart", "stop/start"]:
                if action == "restart":
                    ctl("restart", daemon)
                else:
                    ctl("stop", daemon)
                    assert identity() == original
                    assert prop(owner, "ActiveState") == "active"
                    ctl("start", daemon)
                wait_for(ready, "restarted control socket")
                current_daemon = process_identity(prop(daemon, "MainPID"))
                assert current_daemon and current_daemon != previous_daemon
                previous_daemon = current_daemon
                assert identity() == original, "server or agent process was replaced"
                wait_for(lambda: info() is not None, "re-adopted pane")
                assert {key: info()[key] for key in saved} == saved
                assert prop(scope, "PartOf") == owner
                exchange_prompt(action)
                print(f"PASS {action}: same server, agent PID, session, and working prompt exchange.", flush=True)
        except BaseException:
            print(run("journalctl", "--user", "-u", daemon, "-u", owner, "--no-pager", "-n", "100"),
                  flush=True)
            raise
        finally:
            for unit in [daemon, owner]:
                subprocess.run(["systemctl", "--user", "stop", unit], capture_output=True, timeout=30)
            subprocess.run([*tmux, "kill-server"], capture_output=True, timeout=10)
            for path in files:
                path.unlink(missing_ok=True)
            ctl("daemon-reload")
            for unit in [daemon, owner]:
                subprocess.run(["systemctl", "--user", "reset-failed", unit], capture_output=True, timeout=10)


if __name__ == "__main__":
    exercise(with_companion=False)
    exercise(with_companion=True)

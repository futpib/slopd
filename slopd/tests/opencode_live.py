"""Exercise the actual OpenCode TUI without credentials or a remote model.

Launched by opencode_live.rs; all sessions, processes and tmux sockets are
isolated and removed on success or failure. Only the model response is a fixture.
"""

import http.server
import base64
import json
import os
from pathlib import Path
import queue
import shlex
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request


def wait_for(check, label, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError(f"Timed out waiting for {label}")


def stop(process):
    if process and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def main():
    binary = os.environ["OPENCODE_TEST_BINARY"]
    version = subprocess.check_output([binary, "--version"], text=True).strip()
    v2 = any(word.lstrip("v").startswith("2.") for word in version.split())
    print("Real OpenCode:", version, flush=True)
    with tempfile.TemporaryDirectory(prefix="slopd-opencode-live-") as tmp:
        root = Path(tmp)
        gate = root / "release-tool"
        tool_started = threading.Event()
        requests = []

        class Provider(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                requests.append(body)
                tools = [tool["function"]["name"] for tool in body.get("tools", [])]
                last_user = max((i for i, m in enumerate(body["messages"]) if m.get("role") == "user"), default=-1)
                completed = any(message.get("role") == "tool" for message in body["messages"][last_user + 1:])
                interrupt = "SLOPD_INTERRUPT" in json.dumps(body["messages"][last_user])
                delta = {"role": "assistant", "content": "SLOPD_LIVE_DONE"}
                reason = "stop"
                tool = "shell" if v2 else "bash"
                if tool in tools and not completed:
                    release = root / "interrupt-tool" if interrupt else gate
                    command = f"while [ ! -f {shlex.quote(str(release))} ]; do sleep 0.1; done; printf SLOPD_TOOL_DONE"
                    delta = {"role": "assistant", "tool_calls": [{
                        "index": 0, "id": "call_slopd_live", "type": "function",
                        "function": {"name": tool, "arguments": json.dumps({
                            "command": command, "description": "Wait for test release", "timeout": 120000,
                        })},
                    }]}
                    reason = "tool_calls"
                    tool_started.set()
                data = {
                    "id": "chatcmpl-slopd-live", "object": "chat.completion.chunk",
                    "created": 1, "model": "test",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": None}],
                }
                end = {**data, "choices": [{"index": 0, "delta": {}, "finish_reason": reason}]}
                response = f"data: {json.dumps(data)}\n\ndata: {json.dumps(end)}\n\ndata: [DONE]\n\n".encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(response)))
                self.end_headers()
                self.wfile.write(response)

        provider = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        threading.Thread(target=provider.serve_forever, daemon=True).start()
        env = {k: v for k, v in os.environ.items() if not k.startswith(("OPENCODE_", "TMUX", "SLOP"))}
        env.pop("CODEX_HOME", None)
        env.pop("CLAUDE_CONFIG_DIR", None)
        for key, name in [("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"),
                          ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state"),
                          ("XDG_RUNTIME_DIR", "runtime")]:
            (root / name).mkdir(mode=0o700)
            env[key] = str(root / name)
        env.update({
            "OPENCODE_CONFIG": str(root / "opencode.json"),
            "OPENCODE_CONFIG_DIR": str(root / "config/opencode"),
            "OPENCODE_TEST_HOME": tmp,
            "OPENCODE_TEST_MANAGED_CONFIG_DIR": str(root / "managed"),
            "OPENCODE_DISABLE_PROJECT_CONFIG": "true",
            "OPENCODE_DISABLE_DEFAULT_PLUGINS": "true",
            "OPENCODE_SERVER_PASSWORD": "",
            "OPENCODE_PASSWORD": "",
            "OPENCODE_DISABLE_MODELS_FETCH": "true",
            "OPENCODE_DISABLE_CLAUDE_CODE": "true",
        })
        (root / "opencode.json").write_text(json.dumps({
            "autoupdate": False, "plugin": [], "permission": {"*": "allow", "shell": "ask"} if v2 else "allow",
            "enabled_providers": ["slopd-test"], "model": "slopd-test/test",
            "small_model": "slopd-test/test",
            "provider": {"slopd-test": {
                "npm": "@ai-sdk/openai-compatible", "name": "Slopd test",
                "options": {"baseURL": f"http://127.0.0.1:{provider.server_port}/v1", "apiKey": "local-test"},
                "models": {"test": {"name": "Test", "limit": {"context": 32768, "output": 1024}}},
            }},
        }))
        tmux_socket = str(root / "tmux.sock")
        # A bad slopd config must never fall back to the user's tmux server.
        # Enforce the private socket at the executable boundary, including all
        # tmux calls made by the daemon and its injected hooks.
        real_tmux = shutil.which("tmux")
        assert real_tmux
        (root / "bin").mkdir()
        guard = root / "bin/tmux"
        guard.write_text(
            '#!/bin/sh\n'
            f'if [ "$1" != "-S" ] || [ "$2" != {shlex.quote(tmux_socket)} ]; then\n'
            '  echo "Rejected tmux command outside private test socket" >&2\n  exit 99\nfi\n'
            f'exec {shlex.quote(real_tmux)} "$@"\n'
        )
        guard.chmod(0o700)
        env["PATH"] = str(root / "bin") + os.pathsep + env["PATH"]
        rejected = subprocess.run([str(guard), "list-sessions"], env=env, capture_output=True)
        assert rejected.returncode == 99, "tmux isolation guard failed"
        control = str(root / "slopd.sock")
        config = root / "slopd.toml"
        config.write_text(
            f'config_dir = {json.dumps(str(root / "claude"))}\n'
            f'default_account = "oc"\n[tmux]\nsocket = {json.dumps(tmux_socket)}\n'
            f'[run]\nslopctl = {json.dumps(os.environ["SLOPCTL_TEST_BINARY"])}\n'
            f'start_directory = {json.dumps(tmp)}\n'
            f'[accounts.oc]\nbackend = "opencode"\nexecutable = {json.dumps(["env", binary] if v2 else binary)}\n'
            '[backup]\nauto_backup = false\nauto_restore = false\n'
        )

        def run(args, **kwargs):
            result = subprocess.run(args, env=env, cwd=tmp, text=True, capture_output=True,
                                    timeout=90, **kwargs)
            if result.returncode:
                raise AssertionError(f"Command {args!r} failed: {result.stdout}\n{result.stderr}")
            return result.stdout.strip()

        def tmux(*args):
            return run(["tmux", "-S", tmux_socket, *args])

        cli = [os.environ["SLOPCTL_TEST_BINARY"], "--config", str(config), "--socket", control]
        daemon = listener = None
        pane = None
        log = (root / "slopd.log").open("w+")
        events = []
        event_queue = queue.Queue()

        def start_daemon():
            process = subprocess.Popen([os.environ["SLOPD_TEST_BINARY"], "--config", str(config),
                                        "--socket", control, "-vv"], env=env, cwd=tmp,
                                       stdout=log, stderr=log)
            def ready():
                assert process.poll() is None, "slopd exited"
                with socket.socket(socket.AF_UNIX) as probe:
                    try:
                        probe.connect(control)
                        return True
                    except OSError:
                        return False
            wait_for(ready, "slopd socket")
            assert process.poll() is None, "slopd exited"
            return process

        def info(target=None):
            return next(p for p in json.loads(run(cli + ["ps", "--json"])) if p["pane_id"] == (target or pane))

        def api(path, body=None, method=None):
            headers = {}
            if v2:
                path = "/api/session/active" if path == "/session/status" else "/api" + path
                headers["Authorization"] = "Basic " + base64.b64encode(f"opencode:{token}".encode()).decode()
            headers["Content-Type"] = "application/json"
            with urllib.request.urlopen(urllib.request.Request(f"http://127.0.0.1:{port}{path}", headers=headers,
                                         data=json.dumps(body).encode() if body is not None else None,
                                         method=method), timeout=5) as response:
                if response.status == 204:
                    return None
                result = json.load(response)
                return result["data"] if v2 else result

        def read_events():
            for line in listener.stdout:
                record = json.loads(line)
                events.append(record)
                event_queue.put(record)

        try:
            run(["tmux", "-S", tmux_socket, "-f", "/dev/null", "new-session", "-d", "-s", "test", "-x", "160", "-y", "50"])
            daemon = start_daemon()
            if v2:
                # Reproduce the user's saved-tab leak with a real TUI, then use
                # explicit opt-in as the positive control for the same history.
                cli_config = root / "config/opencode/cli.json"
                cli_config.parent.mkdir(parents=True, exist_ok=True)
                saved_config = '{"animations":false,"session":{"sidebar":"show"}}\n'
                cli_config.write_text(saved_config)
                tabs_on = 'OPENCODE_CLI_CONFIG_CONTENT={"tabs":{"mode":"on"}}'
                pane = run(cli + ["run", "--ready-timeout", "60", "--env", tabs_on])
                port = int(tmux("show-options", "-pv", "-t", pane, "@slopd_opencode_port"))
                token = tmux("show-options", "-pqv", "-t", pane, "@slopd_opencode_token")
                seed_session = info()["session_id"]
                api(f"/session/{seed_session}", {"title": "SLOPD_SAVED_TAB"}, method="PATCH")
                wait_for(lambda: "SLOPD_SAVED_TAB" in tmux("capture-pane", "-p", "-t", pane),
                         "saved tab in seed TUI", 60)
                tabs_file = wait_for(lambda: next((file for file in (root / "state").rglob("tabs.json")
                    if seed_session in file.read_text()), None), "persisted seed tab")
                run(cli + ["kill", pane])
                saved_tabs = tabs_file.read_bytes()

            launch = ["run", "--ready-timeout", "60"]
            if v2:
                launch += ["--env", 'OPENCODE_CLI_CONFIG_CONTENT={"session":{"sidebar":"hide"},"tabs":{"layout":"horizontal"}}']
            pane = run(cli + launch)
            port = int(tmux("show-options", "-pv", "-t", pane, "@slopd_opencode_port"))
            token = tmux("show-options", "-pqv", "-t", pane, "@slopd_opencode_token")
            initial_session = info()["session_id"]
            listener = subprocess.Popen(cli + ["listen"], env=env,
                                        stdout=subprocess.PIPE, stderr=log, text=True)
            threading.Thread(target=read_events, daemon=True).start()
            assert event_queue.get(timeout=10).get("subscribed"), "listener did not subscribe"
            # Finish initial TUI boot/selection before driving native navigation.
            wait_for(lambda: "Test" in tmux("capture-pane", "-p", "-t", pane), "TUI model display", 60)
            time.sleep(3)
            if v2:
                screen = tmux("capture-pane", "-p", "-t", pane)
                assert "SLOPD_SAVED_TAB" not in screen, screen
                assert "Context" not in screen, "existing sidebar override was discarded"
                assert tabs_file.read_bytes() == saved_tabs, "tabs-off launch changed saved tabs"
                assert cli_config.read_text() == saved_config, "launch changed user CLI config"
                probe = run(cli + ["run", "--ready-timeout", "60", "--env", tabs_on])
                wait_for(lambda: "SLOPD_SAVED_TAB" in tmux("capture-pane", "-p", "-t", probe),
                         "saved tab restored by explicit opt-in", 60)
                run(cli + ["kill", probe])
                print("Saved tabs hidden by default; explicit opt-in restores them; CLI overrides and saved files preserved", flush=True)
            # Native navigation creates no tui.session.select event. The next
            # prompt creates a fresh session unknown to slopd's original binding.
            tmux("send-keys", "-t", pane, "C-x", "n")
            time.sleep(0.5)
            run(cli + ["send", pane, "SLOPD_LIVE_PROMPT: run the controlled bash tool."])
            wait_for(tool_started.is_set, "model tool request", 45)
            active = wait_for(lambda: next((sid for sid, status in api("/session/status").items()
                                           if status.get("type") in ("busy", "running")), None), "active OpenCode session")
            assert active != initial_session, "native TUI navigation did not create a different session"
            wait_for(lambda: info()["session_id"] == active, "slopd binding to native TUI session", 10)
            if v2:
                permission = wait_for(lambda: next(iter(api(f"/session/{active}/permission")), None), "permission request")
                wait_for(lambda: info()["detailed_state"] == "awaiting_input_permission", "permission state")
                time.sleep(4)
                assert info()["detailed_state"] == "awaiting_input_permission", info()
                api(f'/session/{active}/permission/{permission["id"]}/reply', {"decision": "always"})
                print("Permission prompt remains visible across status polling", flush=True)
            wait_for(lambda: any(e.get("event_type") == "PreToolUse" for e in events), "live tool hook")
            wait_for(lambda: any(e.get("event_type") == "StateChange"
                                 and e["payload"]["state"] == "busy" for e in events), "busy state event")
            wait_for(lambda: any(e.get("event_type") == "DetailedStateChange"
                                 and e["payload"]["detailed_state"] == "busy_tool_use" for e in events), "tool state event")
            wait_for(lambda: any(e.get("source") == "transcript" and "SLOPD_LIVE_PROMPT" in json.dumps(e)
                                 for e in events), "live user transcript")
            wait_for(lambda: info()["state"] == "busy", "running tool after permission reply")
            assert "SLOPD_LIVE_PROMPT" in run(cli + ["transcript", pane])
            print("Native session switch recovered; busy/tool/transcript events observed", flush=True)

            # Reproduce the long-running stale binding from the bug, including
            # daemon adoption of old durable metadata while the TUI is busy.
            stop(listener)
            stop(daemon)
            tmux("set-option", "-p", "-t", pane, "@slopd_session_id", initial_session)
            daemon = start_daemon()
            wait_for(lambda: info()["session_id"] == active and info()["state"] == "busy",
                     "recovery from stale persisted binding", 15)
            events.clear()
            listener = subprocess.Popen(cli + ["listen"], env=env,
                                        stdout=subprocess.PIPE, stderr=log, text=True)
            threading.Thread(target=read_events, daemon=True).start()
            wait_for(lambda: any(e.get("subscribed") for e in events), "listener after recovery")
            hold_seconds = float(os.environ.get("OPENCODE_TEST_TOOL_HOLD_SECONDS", "31"))
            time.sleep(hold_seconds)
            assert info()["state"] == "busy", info()
            if v2:
                assert info()["detailed_state"] == "busy_tool_use", info()
            gate.touch()
            wait_for(lambda: info()["state"] == "ready", "idle after real bash tool", 30)
            transcript = run(cli + ["transcript", pane])
            assert "SLOPD_TOOL_DONE" in transcript and "SLOPD_LIVE_DONE" in transcript, transcript
            wait_for(lambda: any(e.get("source") == "transcript" and "SLOPD_LIVE_DONE" in json.dumps(e)
                                 for e in events), "live transcript after recovered binding")
            assert any(e.get("event_type") == "PostToolUse" for e in events), events
            assert any(e.get("event_type") == "Stop" for e in events), events
            wait_for(lambda: any(e.get("event_type") == "StateChange"
                                 and e["payload"]["state"] == "ready" for e in events), "idle state event")
            assert tmux("show-options", "-pv", "-t", pane, "@slopd_session_id") == active
            log.flush()
            assert "recovered stale session binding" in (root / "slopd.log").read_text()
            print(f"Stale-binding restart recovered; tool held {hold_seconds}s and state returned to ready", flush=True)

            tool = "shell" if v2 else "bash"
            assert any(e.get("event_type") == "PostToolUse" and e["payload"]["extra"]["tool_name"] == tool
                       for e in events), "tool identity lost across daemon restart"

            fork = run(cli + ["fork", pane, "--ready-timeout", "60"])
            fork_session = info(fork)["session_id"]
            assert fork_session != active
            assert "SLOPD_LIVE_DONE" in run(cli + ["transcript", fork])
            wait_for(lambda: "Test" in tmux("capture-pane", "-p", "-t", fork), "fork TUI")
            if v2:
                assert "SLOPD_SAVED_TAB" not in tmux("capture-pane", "-p", "-t", fork)
            time.sleep(1)
            tmux("send-keys", "-t", fork, "-l", "DISCARD_STALE_COMPOSER")
            prompt = "SLOPD_MULTILINE\n" + "quoted `literal` $value \'text\' " * 800 + "\nEND_MULTILINE"
            run(cli + ["send", fork, prompt])
            wait_for(lambda: "END_MULTILINE" in run(cli + ["transcript", fork]), "large multiline prompt")
            wait_for(lambda: info(fork)["state"] == "ready", "fork completion")
            assert "DISCARD_STALE_COMPOSER" not in run(cli + ["transcript", fork])
            assert "SLOPD_MULTILINE" not in run(cli + ["transcript", pane]), "fork changed source conversation"

            tool_started.clear()
            run(cli + ["send", fork, "SLOPD_INTERRUPT: run the controlled tool until interrupted."])
            wait_for(tool_started.is_set, "interrupt tool request")
            wait_for(lambda: info(fork)["state"] == "busy", "busy before interrupt")
            run(cli + ["interrupt", fork])
            wait_for(lambda: info(fork)["state"] == "ready", "ready after interrupt")
            print("Fork history, independent multiline input, and API interrupt passed", flush=True)

            # A fresh TUI must find the same persisted conversation after a full
            # tmux-server loss, not merely after reconnecting to the old process.
            run(cli + ["backup"])
            stop(listener)
            old_ports = [int(tmux("show-options", "-pv", "-t", target, "@slopd_opencode_port")) for target in (pane, fork)]
            tmux("kill-server")
            def servers_closed():
                for old_port in old_ports:
                    with socket.socket() as probe:
                        probe.settimeout(0.2)
                        if probe.connect_ex(("127.0.0.1", old_port)) == 0:
                            return False
                return True
            wait_for(servers_closed, "private servers stopped with tmux", 15)
            wait_for(lambda: json.loads(run(cli + ["ps", "--json"])) == [], "old panes gone")
            run(cli + ["restore"])
            def restored():
                rows = json.loads(run(cli + ["ps", "--json"]))
                return rows if len(rows) == 2 and all(p["state"] == "ready" for p in rows) else None
            rows = wait_for(restored, "restored sessions", 60)
            assert {p["session_id"] for p in rows} == {active, fork_session}, rows
            for row in rows:
                transcript = run(cli + ["transcript", row["pane_id"]])
                assert "SLOPD_LIVE_DONE" in transcript
                assert ("SLOPD_MULTILINE" in transcript) == (row["session_id"] == fork_session)
                if v2:
                    wait_for(lambda: "Test" in tmux("capture-pane", "-p", "-t", row["pane_id"]), "restored TUI", 60)
                    assert "SLOPD_SAVED_TAB" not in tmux("capture-pane", "-p", "-t", row["pane_id"])
            print("Full tmux-server loss restored both exact session histories", flush=True)
            upgrade = os.environ.get("OPENCODE_TEST_UPGRADE_BINARY")
            if upgrade and not v2:
                run(cli + ["backup"])
                tmux("kill-server")
                wait_for(lambda: json.loads(run(cli + ["ps", "--json"])) == [], "v1 panes gone before upgrade")
                stop(daemon)
                config.write_text(config.read_text().replace(
                    f'executable = {json.dumps(binary)}', f'executable = {json.dumps(upgrade)}'))
                run(["tmux", "-S", tmux_socket, "-f", "/dev/null", "new-session", "-d", "-s", "test", "-x", "160", "-y", "50"])
                daemon = start_daemon()
                run(cli + ["restore"])
                rows = wait_for(restored, "v1 histories restored through v2 migration", 60)
                assert {p["session_id"] for p in rows} == {active, fork_session}, rows
                for row in rows:
                    transcript = run(cli + ["transcript", row["pane_id"]])
                    assert "SLOPD_LIVE_DONE" in transcript
                    assert ("SLOPD_MULTILINE" in transcript) == (row["session_id"] == fork_session)
                print("Upgrade to OpenCode 2 preserved both v1 session identities and histories", flush=True)
        except Exception:
            if pane:
                screen = subprocess.run(["tmux", "-S", tmux_socket, "capture-pane", "-p", "-t", pane],
                                        env=env, capture_output=True, text=True, timeout=5)
                print("TUI:\n", screen.stdout, screen.stderr, flush=True)
            log.flush()
            print("Daemon log:\n", (root / "slopd.log").read_text()[-14000:], flush=True)
            print("Provider requests:", len(requests), "events:", json.dumps(events)[-5000:], flush=True)
            raise
        finally:
            gate.touch()
            (root / "interrupt-tool").touch()
            stop(listener)
            stop(daemon)
            subprocess.run(["tmux", "-S", tmux_socket, "kill-server"], env=env,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
            provider.shutdown()
            provider.server_close()
            log.close()


if __name__ == "__main__":
    main()

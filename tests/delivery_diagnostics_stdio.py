#!/usr/bin/env python3
"""Real broker/socket/stdio diagnostics contract. No live state or HTTP callbacks.

Usage: python3 tests/delivery_diagnostics_stdio.py /absolute/path/dev-session-mcp
Exit 77 means the environment blocked setup; it is NOT a passing connection test.
All subscription keys/bodies below are synthetic redaction canaries, never credentials.
"""
import copy
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import sys
import tempfile
import time


META = {
    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
    "io.modelcontextprotocol/clientInfo": {"name": "diagnostics-contract", "version": "1"},
    "io.modelcontextprotocol/clientCapabilities": {},
}
CANARY = "DIAGNOSTICS_PRIVATE_CANARY"


class EnvironmentBlocked(Exception):
    pass


def private_write(path, data):
    # Only create our own new fixture. Never replace an existing user's store.
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as f:
        f.write(data)


def broker_request(state, **request):
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(5)
        stream.connect(str(state / "broker.sock"))
        stream.sendall((json.dumps(request) + "\n").encode())
        result = json.loads(stream.makefile("rb").readline())
        assert result["ok"], "test broker rejected read/control request"
        return result["result"]


class Frontend:
    def __init__(self, binary, env):
        self.process = subprocess.Popen(
            [binary, "stdio"], env=env, stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        self.sequence = 0

    def request(self, method, params=None):
        self.sequence += 1
        request = {"jsonrpc": "2.0", "id": self.sequence, "method": method,
                   "params": dict(params or {}, _meta=META)}
        self.process.stdin.write((json.dumps(request) + "\n").encode())
        self.process.stdin.flush()
        assert select.select([self.process.stdout], [], [], 10)[0], "stdio response timed out"
        line = self.process.stdout.readline()
        assert line, "stdio frontend exited without a response"
        response = json.loads(line)
        assert response["id"] == self.sequence
        return response

    def tool(self, name, **arguments):
        response = self.request("tools/call", {"name": name, "arguments": arguments})
        assert "error" not in response, "unexpected JSON-RPC error"
        result = response["result"]
        assert not result.get("isError"), "unexpected tool error"
        data = result["structuredContent"]
        assert json.loads(result["content"][0]["text"]) == data, "content channels disagree"
        return data

    def diagnostic(self, **arguments):
        value = self.tool("read_delivery_diagnostics", **arguments)
        assert CANARY not in json.dumps(value), "private fixture leaked"
        assert set(value) == {"observed_at_ms", "session_id", "session_state", "execution", "subscriptions"}
        return value

    def bad_diagnostic(self, arguments):
        response = self.request("tools/call", {"name": "read_delivery_diagnostics", "arguments": arguments})
        assert "error" in response or response.get("result", {}).get("isError"), "bad arguments accepted"
        assert CANARY not in json.dumps(response), "error leaked private input"

    def close(self):
        if self.process.poll() is None:
            self.process.stdin.close()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
                raise AssertionError("stdio frontend did not exit")
        assert self.process.returncode == 0, "stdio frontend failed"

    def cleanup(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        for pipe in (self.process.stdin, self.process.stdout, self.process.stderr):
            pipe.close()


def fixtures(session, target, sibling, other_session):
    now = int(time.time())
    history = [{
        "event_id": f"fixture-event-{i}", "cursor": CANARY + "_history_cursor",
        "queued_at_ms": 100000 + i, "first_event_at": "2026-10-08T01:00:00.123Z",
        "last_event_at": "2026-10-08T01:00:00.456Z", "attempt": 1,
        "started_at_ms": 100100 + i, "finished_at_ms": 100440 + i,
        "elapsed_ms": 340, "status": 403,
    } for i in range(20)]
    template = {
        "id": "fixture-project", "owner": os.geteuid(),
        "url": "https://fixture.invalid/" + CANARY, "secret": CANARY + "_key",
        "old_secret": [CANARY + "_old_key", now + 3600],
        "session_id": session, "execution_id": None, "expires": now + 3600,
        "cursor": CANARY + "_cursor", "suspended": True, "delivery_history": history,
        "pending": {"event_id": "fixture-pending", "body": CANARY + "_body",
                    "cursor": CANARY + "_pending_cursor", "attempts": 2, "queued_at_ms": 100000},
    }
    values = {}
    for name, changes in {
        "project": {}, "target": {"execution_id": target},
        "sibling": {"execution_id": sibling}, "other": {"session_id": other_session},
        "foreign": {"owner": os.geteuid() ^ 1}, "expired": {"expires": now - 1},
        "legacy": {"execution_id": target},
    }.items():
        value = copy.deepcopy(template)
        value.update(changes, id="fixture-" + name)
        if name == "legacy":
            del value["delivery_history"]
            del value["pending"]["queued_at_ms"]
        values[value["id"]] = value
    # Every retained fixture is suspended: Events::load must launch no sender.
    assert all(s["suspended"] for s in values.values())
    return values


def run(binary):
    with tempfile.TemporaryDirectory(prefix="delivery-diagnostics-stdio-") as directory:
        root = Path(directory)
        state = root / "state"
        (root / "tmp").mkdir(mode=0o700)
        projects = [root / name for name in ("project", "other", "empty")]
        for project in projects:
            project.mkdir(mode=0o700)
        # Do not inherit a broker override, transport credentials, proxies or live state.
        env = {"PATH": os.defpath, "LANG": "C.UTF-8", "TERM": "dumb",
               "TMPDIR": str(root / "tmp"), "DEV_SESSION_MCP_STATE_DIR": str(state)}
        broker = subprocess.Popen([binary, "broker"], env=env, stdin=subprocess.DEVNULL,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        frontends = []
        try:
            deadline = time.monotonic() + 5
            while not (state / "broker.sock").exists():
                if broker.poll() is not None:
                    error = broker.stderr.read().decode(errors="replace")
                    if "Operation not permitted" in error or "Permission denied" in error:
                        raise EnvironmentBlocked("real broker socket setup denied; socket/stdio assertions NOT RUN")
                    raise AssertionError("test broker exited before socket creation")
                if time.monotonic() > deadline:
                    raise AssertionError("test broker socket startup timed out")
                time.sleep(0.02)
            assert (state / "broker.sock").stat().st_mode & 0o777 == 0o600
            assert (state / "broker.sock").stat().st_uid == os.geteuid()
            try:
                ping = broker_request(state, op="ping")
            except PermissionError as error:
                raise EnvironmentBlocked("real Unix socket connection denied; stdio assertions NOT RUN") from error
            assert ping["pid"] == broker.pid, "connected to a different broker"
            client = Frontend(binary, env)
            frontends.append(client)
            discovery = client.request("server/discover")["result"]
            assert discovery["capabilities"]["events"] == {}
            tools = client.request("tools/list")["result"]["tools"]
            tool = next(t for t in tools if t["name"] == "read_delivery_diagnostics")
            assert tool["annotations"]["readOnlyHint"] and not tool["annotations"]["destructiveHint"]
            assert tool["inputSchema"]["required"] == ["session_id"]
            assert tool["inputSchema"]["additionalProperties"] is False
            sessions = [client.tool("open_session", cwd=str(p))["id"] for p in projects]
            session, other, empty = sessions
            jobs = [client.tool("start_execution", session_id=s, command=["/bin/cat"],
                                profile="host", io="pipes") for s in (session, session, other)]
            target, sibling, foreign_job = [job["execution_id"] for job in jobs]
            assert client.diagnostic(session_id=empty)["subscriptions"] == []
            client.close()
            store_path = state / "events" / "subscriptions.json"
            expected = json.dumps(fixtures(session, target, sibling, other), sort_keys=True).encode()
            private_write(store_path, expected)
            before_mtime = store_path.stat().st_mtime_ns
            client = Frontend(binary, env)
            frontends.append(client)
            client.request("server/discover")
            position = broker_request(state, op="event_position", session_id=session)
            for _ in range(2):
                value = client.diagnostic(session_id=session, execution_id=target)
                assert value["execution"] == {"execution_id": target, "status": "running", "exit_code": None}
                rows = {s["subscription_id"]: s for s in value["subscriptions"]}
                assert set(rows) == {"fixture-project", "fixture-target", "fixture-legacy"}
                for row in rows.values():
                    assert set(row) == {"subscription_id", "execution_id", "lease_expires_at_unix_seconds",
                        "expired", "suspended", "worker_running", "delivery_state", "has_unacknowledged_batch",
                        "pending_queued_at_ms", "last_http_status", "delivery_history"}
                    assert row["suspended"] and not row["worker_running"] and not row["expired"]
                    assert row["delivery_state"] == "suspended" and row["has_unacknowledged_batch"]
                row = rows["fixture-target"]
                assert row["last_http_status"] == 403 and len(row["delivery_history"]) == 16
                assert row["delivery_history"][0]["event_id"] == "fixture-event-4"
                assert row["delivery_history"][-1]["event_id"] == "fixture-event-19"
                assert row["delivery_history"][-1]["elapsed_ms"] == 340
                assert row["delivery_history"][-1]["started_at_ms"] == 100119
                assert row["delivery_history"][-1]["finished_at_ms"] == 100459
                assert set(row["delivery_history"][0]) == {"event_id", "attempt", "queued_at_ms",
                    "first_event_at", "last_event_at", "started_at_ms", "finished_at_ms", "elapsed_ms", "status"}
                assert rows["fixture-legacy"]["delivery_history"] == []
                assert rows["fixture-legacy"]["last_http_status"] is None
                assert rows["fixture-legacy"]["pending_queued_at_ms"] is None
            value = client.diagnostic(session_id=session)
            assert value["execution"] is None and len(value["subscriptions"]) == 4
            value = client.diagnostic(session_id=session, execution_id=sibling)
            assert {s["subscription_id"] for s in value["subscriptions"]} == {"fixture-project", "fixture-sibling"}
            assert client.diagnostic(session_id=empty)["subscriptions"] == []
            for args in ({}, {"session_id": None}, {"session_id": 7}, {"session_id": "../invalid"},
                         {"session_id": "missing"}, {"session_id": session, "execution_id": foreign_job},
                         {"session_id": session, "execution_id": "missing"},
                         {"session_id": session, "execution_id": None},
                         {"session_id": session, "owner": os.geteuid()},
                         {"session_id": session, "url": CANARY}, {"session_id": session, "secret": CANARY}):
                client.bad_diagnostic(args)
            unknown = client.request("events/diagnostics", {"session_id": session})
            assert unknown["error"]["code"] == -32601, "invented Events method accepted"
            assert broker_request(state, op="event_position", session_id=session) == position
            assert store_path.read_bytes() == expected and store_path.stat().st_mtime_ns == before_mtime
            assert not list(store_path.parent.glob("*.tmp")), "diagnostic attempted persistence"
            closed = client.tool("close_session", session_id=session)
            assert closed["closed"] and not closed["pending"]
            value = client.diagnostic(session_id=session, execution_id=target)
            assert value["session_state"] == "closed" and value["execution"]["status"] == "exited"
            assert len(value["subscriptions"]) == 3, "retained terminal lease missing"
            client.tool("close_session", session_id=other)
            client.tool("close_session", session_id=empty)
            assert store_path.read_bytes() == expected, "diagnostic changed retained pending batches"
            client.close()
            broker_request(state, op="shutdown")
            broker.wait(timeout=5)
            assert broker.returncode == 0
            print("PASS: real owned Unix broker + stdio discovery/tool dispatch; read-only diagnostics; "
                  "redaction; invalid input/session/execution/owner boundaries; legacy and bounded history; "
                  "frontend restart; closed-session retention. No HTTP or chat delivery tested.")
        finally:
            for client in frontends:
                client.cleanup()
            if broker.poll() is None:
                # Only the process we spawned, never a PID loaded from any state file.
                broker.terminate()
                try:
                    broker.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    broker.kill()
                    broker.wait()
            broker.stderr.close()


if __name__ == "__main__":
    try:
        if len(sys.argv) != 2:
            raise ValueError("pass the locally built server binary")
        run(str(Path(sys.argv[1]).resolve(strict=True)))
    except EnvironmentBlocked as error:
        print("BLOCKED:", error)
        sys.exit(77)

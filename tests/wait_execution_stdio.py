#!/usr/bin/env python3
"""Isolated real stdio tests; optional second argument is an unchanged old broker binary."""
import concurrent.futures
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from delivery_diagnostics_stdio import Frontend, META, broker_request


def run(binary, broker_binary):
    with tempfile.TemporaryDirectory(prefix="wait-execution-") as directory:
        root = Path(directory)
        state = root / "state"
        (root / "tmp").mkdir()
        (root / "project").mkdir()
        env = {"PATH": os.defpath, "LANG": "C.UTF-8", "TMPDIR": str(root / "tmp"),
               "DEV_SESSION_MCP_STATE_DIR": str(state),
               "DEV_SESSION_MCP_BROKER_SOCKET": str(state / "broker.sock")}
        broker = subprocess.Popen([broker_binary, "broker"], env=env, stdout=subprocess.DEVNULL,
                                  stderr=subprocess.PIPE)
        client = None
        try:
            deadline = time.monotonic() + 5
            while not (state / "broker.sock").exists():
                assert broker.poll() is None and time.monotonic() < deadline
                time.sleep(.02)
            assert broker_request(state, op="ping")["pid"] == broker.pid
            client = Frontend(binary, env)
            client.request("server/discover")
            definition = next(t for t in client.request("tools/list")["result"]["tools"]
                              if t["name"] == "wait_execution")
            assert definition["annotations"]["readOnlyHint"]
            assert definition["inputSchema"]["required"] == ["execution_id", "cursor"]
            session = client.tool("open_session", cwd=str(root / "project"))["id"]
            job = client.tool("start_execution", session_id=session, command=["/bin/cat"], profile="host", io="pipes")
            target = job["execution_id"]
            cursor = job["cursor"]

            def wait(position, ms=1000):
                return client.tool("wait_execution", execution_id=target, cursor=position, max_wait_ms=ms)

            invalid = [{}, {"execution_id": target}, {"execution_id": target, "cursor": "invalid"},
                       {"execution_id": "missing", "cursor": cursor},
                       {"execution_id": target, "cursor": cursor, "secret": "SYNTHETIC_CANARY"}]
            invalid += [dict(execution_id=target, cursor=cursor, max_wait_ms=v)
                        for v in (-1, 10001, 1.5, None, "1", True)]
            for args in invalid:
                result = client.request("tools/call", {"name": "wait_execution", "arguments": args})
                assert "error" in result or result["result"].get("isError"), result
                assert "SYNTHETIC_CANARY" not in json.dumps(result)
            began = time.monotonic()
            result = wait(cursor, 120)
            elapsed = (time.monotonic() - began) * 1000
            assert result["timed_out"] and not result["events"] and result["execution"]["status"] == "running"
            assert 100 <= elapsed < 1500
            print(f"timeout(120ms): {elapsed:.2f} ms")
            cursor = result["cursor"]
            # Unrelated global sequences must advance the returned cursor without waking early.
            sibling = client.tool("start_execution", session_id=session, command=["/bin/cat"], profile="host", io="pipes")
            result = wait(cursor, 80)
            assert result["timed_out"] and result["events"] == [] and result["cursor"] != cursor
            cursor = result["cursor"]
            # Concurrent caller control wakes the wait; no execution/input retry by the tool.
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                future = pool.submit(wait, cursor, 2000)
                time.sleep(.08)
                sent = time.monotonic()
                broker_request(state, op="input_execution", execution_id=target, text="once\n")
                result = future.result(timeout=4)
                print(f"control event -> local tool response: {(time.monotonic()-sent)*1000:.2f} ms")
            assert not result["timed_out"] and result["events"]
            # Drain finite event pages until cat's output arrives, then ensure no duplicate.
            events = result["events"][:]
            cursor = result["cursor"]
            for _ in range(8):
                if any(e["kind"] == "output" for e in events):
                    break
                result = wait(cursor)
                events += result["events"]
                cursor = result["cursor"]
            assert "".join(e["data"].get("text", "") for e in events if e["kind"] == "output") == "once\n"
            assert len({e["sequence"] for e in events}) == len(events)
            assert wait(cursor, 0)["events"] == []
            # Cancellation releases both frontend permit and broker socket/slot (40 > 32).
            for _ in range(40):
                client.sequence += 1
                request_id = client.sequence
                request = {"jsonrpc": "2.0", "id": request_id, "method": "tools/call",
                           "params": {"name": "wait_execution", "arguments": {"execution_id": target,
                                      "cursor": cursor, "max_wait_ms": 10000}, "_meta": META}}
                client.process.stdin.write((json.dumps(request) + "\n").encode())
                client.process.stdin.flush()
                time.sleep(.01)
                client.process.stdin.write((json.dumps({"jsonrpc": "2.0", "method": "notifications/cancelled",
                    "params": {"requestId": request_id, "reason": "fixture cancellation"}}) + "\n").encode())
                client.process.stdin.flush()
                client.request("ping")  # SDK suppresses the cancelled request response.
            assert wait(cursor, 0)["timed_out"]
            assert broker_request(state, op="read_execution", execution_id=target)["execution"]["status"] == "running"
            # Transport EOF must drop the pending wait; reconnect retains the same job.
            client.sequence += 1
            client.process.stdin.write((json.dumps({"jsonrpc": "2.0", "id": client.sequence,
                "method": "tools/call", "params": {"name": "wait_execution", "arguments": {
                    "execution_id": target, "cursor": cursor, "max_wait_ms": 10000}, "_meta": META}}) + "\n").encode())
            client.process.stdin.flush()
            time.sleep(.03)
            # rmcp 3.5.1 drains in-flight handlers for up to 5 s on EOF.
            disconnected_at = time.monotonic()
            client.process.stdin.close()
            client.process.wait(timeout=7)
            assert client.process.returncode == 0
            print(f"transport EOF -> frontend exit: {(time.monotonic()-disconnected_at)*1000:.2f} ms (SDK drain)")
            client.cleanup()
            client = Frontend(binary, env)
            client.request("server/discover")
            assert wait(cursor, 0)["execution"]["execution_id"] == target
            # Four pending waits leave control responsive; a fifth is rejected, not queued.
            pending = []
            for _ in range(4):
                client.sequence += 1
                pending.append(client.sequence)
                request = {"jsonrpc": "2.0", "id": client.sequence, "method": "tools/call",
                           "params": {"name": "wait_execution", "arguments": {"execution_id": target,
                                      "cursor": cursor, "max_wait_ms": 10000}, "_meta": META}}
                client.process.stdin.write((json.dumps(request) + "\n").encode())
                client.process.stdin.flush()
            time.sleep(.1)
            rejected = client.request("tools/call", {"name": "wait_execution", "arguments": {
                "execution_id": target, "cursor": cursor, "max_wait_ms": 0}})
            assert rejected["result"]["isError"]
            assert "capacity" in rejected["result"]["content"][0]["text"]
            for request_id in pending:
                client.process.stdin.write((json.dumps({"jsonrpc": "2.0", "method": "notifications/cancelled",
                    "params": {"requestId": request_id}}) + "\n").encode())
            client.process.stdin.flush()
            client.request("ping")
            assert wait(cursor, 0)["timed_out"]
            # Independent generated timestamp measures output-generation -> local tool completion.
            timed_job = client.tool("start_execution", session_id=session,
                command=[sys.executable, "-c", "import time; time.sleep(.2); print(time.time_ns(), flush=True)"],
                profile="host", io="pipes")
            timed = client.tool("wait_execution", execution_id=timed_job["execution_id"], cursor=timed_job["cursor"], max_wait_ms=2000)
            received_ns = time.time_ns()
            stamp = int("".join(e["data"].get("text", "") for e in timed["events"] if e["kind"] == "output").strip())
            print(f"output timestamp -> local stdio tool response: {(received_ns-stamp)/1e6:.2f} ms (NOT ChatGPT acceptance)")
            # Produce multiple bounded pages, while the journal retains everything.
            bulk = client.tool("start_execution", session_id=session,
                command=[sys.executable, "-c", "import sys; sys.stdout.write('z'*90000)"], profile="host", io="pipes")
            position = bulk["cursor"]
            body, sequences = "", []
            for _ in range(100):
                page = client.tool("wait_execution", execution_id=bulk["execution_id"], cursor=position, max_wait_ms=500)
                body += "".join(e["data"].get("text", "") for e in page["events"] if e["kind"] == "output")
                sequences += [e["sequence"] for e in page["events"]]
                position = page["cursor"]
                if page["execution"]["status"] == "exited" and not page["more"]:
                    break
            assert body == "z"*90000 and len(sequences) == len(set(sequences))
            for old_cursor in ("previous-epoch:0", position.rsplit(':', 1)[0] + ":999999999"):
                page = client.tool("wait_execution", execution_id=bulk["execution_id"], cursor=old_cursor, max_wait_ms=10000)
                assert page["catch_up_required"] and not page["timed_out"]
            # Global journal truncation must explicitly report loss, never silently reset.
            flood = client.tool("start_execution", session_id=session,
                command=[sys.executable, "-c", "import sys; sys.stdout.write('q'*1200000)"], profile="host", io="pipes")
            pos = flood["cursor"]
            for _ in range(500):
                page = client.tool("wait_execution", execution_id=flood["execution_id"], cursor=pos, max_wait_ms=500)
                pos = page["cursor"]
                if page["execution"]["status"] == "exited" and not page["more"]:
                    break
            assert wait(cursor, 10000)["catch_up_required"]
            client.tool("close_session", session_id=session)
            final = client.tool("read_execution", execution_id=target)
            began = time.monotonic()
            result = wait(final["cursor"], 10000)
            assert result["execution"]["status"] == "exited" and not result["timed_out"]
            assert time.monotonic() - began < 1
            print(f"terminal immediate: {(time.monotonic()-began)*1000:.2f} ms")
            client.close()
            broker_request(state, op="shutdown")
            broker.wait(timeout=5)
            print("PASS: real stdio wait/timeout/cancel, old broker, global cursors, pages/gaps/epoch, terminal; no HTTP/chat delivery")
        finally:
            if client:
                client.cleanup()
            if broker.poll() is None:
                broker.terminate()
                try:
                    broker.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    broker.kill()
                    broker.wait()
            broker.stderr.close()


if __name__ == "__main__":
    run(str(Path(sys.argv[1]).resolve(strict=True)),
        str(Path(sys.argv[2] if len(sys.argv) > 2 else sys.argv[1]).resolve(strict=True)))

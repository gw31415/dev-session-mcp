#!/usr/bin/env python3
"""Isolated recovery regression: never opens live state, credentials, or services."""
import concurrent.futures
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
from urllib.parse import quote
from delivery_diagnostics_stdio import Frontend, broker_request


class RecoveryFrontend(Frontend):
    def tool(self, name, **arguments):
        response = self.request("tools/call", {"name":name,"arguments":arguments})
        assert "error" not in response, response.get("error")
        result = response["result"]
        assert not result.get("isError"), (name, result.get("content"))
        data = result["structuredContent"]
        assert json.loads(result["content"][0]["text"]) == data
        return data

def run(binary):
    with tempfile.TemporaryDirectory(prefix="dev-session-recovery-") as directory:
        root = Path(directory)
        state = root / "state"
        project = root / "project"
        project.mkdir()
        env = {"PATH": os.defpath, "LANG": "C.UTF-8", "DEV_SESSION_MCP_STATE_DIR": str(state),
               "DEV_SESSION_MCP_BROKER_SOCKET": str(state / "broker.sock")}
        processes = []
        clients = []
        read_retries = 0

        def boot():
            p = subprocess.Popen([binary, "broker"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            processes.append(p)
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline:
                assert p.poll() is None, "fixture broker failed"
                try:
                    if broker_request(state, op="ping")["pid"] == p.pid:
                        return p
                except (OSError, AssertionError):
                    pass
                time.sleep(.02)
            raise AssertionError("fixture startup timeout")

        def frontend():
            c = RecoveryFrontend(binary, env)
            clients.append(c)
            c.request("server/discover")
            return c

        def finish(c, job):
            nonlocal read_retries
            cursor = job["cursor"]
            all_events = []
            deadline = time.monotonic() + 20
            for _ in range(200):
                assert time.monotonic() < deadline, "fixture recovery deadline exceeded"
                response = c.request("tools/call", {"name":"wait_execution", "arguments":dict(execution_id=job["execution_id"], cursor=cursor, max_wait_ms=100)})
                assert "error" not in response, response.get("error")
                result = response["result"]
                if result.get("isError"):
                    assert "snapshot deadline exceeded" in result["content"][0]["text"], result["content"]
                    # Only a read is retried, from the SAME last received cursor. Never resend start/input.
                    read_retries += 1
                    time.sleep(.02)
                    continue
                page = result["structuredContent"]
                all_events.extend(page["events"])
                cursor = page["cursor"]
                if page["execution"]["status"] != "running" and not page["more"]:
                    return page, all_events
            raise AssertionError("short fixture did not finish")

        try:
            broker = boot()
            c = frontend()
            discovery = c.request("server/discover")["result"]
            assert "io.modelcontextprotocol/tasks" not in discovery["capabilities"].get("extensions",{})
            catalog = c.request("resources/list")["result"]
            assert catalog["ttlMs"] == 0 and catalog["cacheScope"] == "private"
            assert len(c.request("resources/templates/list")["result"]["resourceTemplates"]) == 2
            session = c.tool("open_session", cwd=str(project))["id"]
            counter = project / "starts"
            args = dict(session_id=session, command=["/bin/sh", "-c", f"echo once >> {counter}; printf 'DETAIL_CANARY'; sleep .1"],
                        profile="host", io="pipes", idempotency_key="lost-ack", work_id="work",
                        purpose="Verify one execution", completion_condition="Counter is exactly one and exit confirmed")
            # Send start, wait for its side effect, deliberately discard the response.
            stream = socket.socket(socket.AF_UNIX)
            stream.connect(str(state / "broker.sock"))
            stream.sendall((json.dumps(dict(args, op="start_execution")) + "\n").encode())
            deadline = time.monotonic() + 5
            while not counter.exists():
                assert time.monotonic() < deadline
                time.sleep(.01)
            stream.close()
            job = c.tool("start_execution", **args)
            assert job["replayed"] and counter.read_text() == "once\n"
            end, replay_events = finish(c, job)
            assert "DETAIL_CANARY" in json.dumps(replay_events), "job.cursor skipped output after ACK loss"
            assert end["execution"]["status"] == "exited"
            job_id = job["execution_id"]
            initial = job["cursor"]
            summary = c.tool("read_execution", execution_id=job_id, cursor=initial, view="summary")
            assert "DETAIL_CANARY" not in json.dumps(summary) and "events" not in summary
            assert summary["detail_cursor"] == initial and summary["state_cursor"] != initial
            implicit = c.tool("read_execution", execution_id=job_id, view="summary")
            assert implicit["detail_cursor"] == job["initial_cursor"] and implicit["detail_available"]
            replay = c.tool("start_execution",**args)
            assert replay["cursor"] == initial
            assert "DETAIL_CANARY" in json.dumps(c.tool("read_execution",execution_id=job_id,cursor=replay["cursor"]))
            detail = c.tool("read_execution", execution_id=job_id, cursor=initial)
            assert "DETAIL_CANARY" in json.dumps(detail)
            assert detail == c.tool("read_execution", execution_id=job_id, cursor=initial)
            uri = summary["details_uri"]
            resource = c.request("resources/read", {"uri":uri})["result"]
            assert resource["cacheScope"] == "private" and resource["ttlMs"] == 0
            assert json.loads(resource["contents"][0]["text"]) == detail
            encoded_uri = f"dev-session:///executions/{quote(job_id,safe='')}?cursor={quote(initial,safe='')}"
            encoded = c.request("resources/read", {"uri":encoded_uri})["result"]
            assert json.loads(encoded["contents"][0]["text"]) == detail
            source = c.request("resources/read", {"uri":f"dev-session:///source/{job_id}"})["result"]
            assert json.loads(source["contents"][0]["text"])["command"] == args["command"]
            for reader in ("one", "two"):
                value = c.tool("checkpoint_execution", execution_id=job_id, reader_id=reader,
                               cursor=initial, expected_revision=0)
                assert value["revision"] == 1
            c.tool("checkpoint_execution", execution_id=job_id, reader_id="one", cursor=detail["cursor"], expected_revision=1)
            conflict = c.request("tools/call", {"name":"checkpoint_execution", "arguments":dict(execution_id=job_id,
                reader_id="one",cursor=detail["cursor"],expected_revision=1)})
            assert conflict["result"]["isError"]
            index = c.tool("open_session", cwd=str(project))
            two = next(v for v in index["checkpoints"] if v["reader_id"] == "two")
            assert two["detail_cursor"] == initial and two["revision"] == 1
            checkpoint_args = dict(execution_id=job_id,reader_id="one",cursor=detail["cursor"],expected_revision=2)
            with socket.socket(socket.AF_UNIX) as lost_checkpoint:
                lost_checkpoint.connect(str(state / "broker.sock"))
                lost_checkpoint.sendall((json.dumps(dict(checkpoint_args,op="checkpoint_execution"))+"\n").encode())
                deadline = time.monotonic() + 5
                while next(v for v in c.tool("list_sessions")["checkpoints"] if v["reader_id"] == "one")["revision"] != 3:
                    assert time.monotonic() < deadline
                    time.sleep(.01)
            retry = c.request("tools/call",{"name":"checkpoint_execution","arguments":checkpoint_args})
            assert retry["result"]["isError"]
            changed = dict(args, command=["/bin/true"])
            assert c.request("tools/call", {"name":"start_execution","arguments":changed})["result"]["isError"]
            # Concurrent retries, including across frontend lifetimes, remain the same execution.
            c.close()
            c = frontend()
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                replies = list(pool.map(lambda _: broker_request(state, op="start_execution", **args), range(2)))
            assert all(v["execution_id"] == job_id for v in replies)
            assert counter.read_text() == "once\n"
            # Drop an input ACK after the write; observe completion without resending stdin.
            input_file = project / "input-once"
            receiver = c.tool("start_execution",session_id=session,command=["/bin/sh","-c",f"cat > {input_file}"],profile="host",io="pipes")
            with socket.socket(socket.AF_UNIX) as lost_input:
                lost_input.connect(str(state / "broker.sock"))
                lost_input.sendall((json.dumps(dict(op="input_execution",execution_id=receiver["execution_id"],text="input once",close_stdin=True))+"\n").encode())
                deadline = time.monotonic() + 5
                while not input_file.exists() or input_file.read_text() != "input once":
                    assert time.monotonic() < deadline
                    time.sleep(.01)
            input_end, _ = finish(c, receiver)
            assert input_end["execution"]["status"] == "exited" and input_file.read_text() == "input once"
            # Summary state cursor advances independently while details remain unread.
            live = c.tool("start_execution", session_id=session, command=["/bin/sh","-c","printf 'unread'; sleep 2"],
                          profile="host",io="pipes",idempotency_key="live")
            time.sleep(.1)
            observation = c.tool("read_execution", execution_id=live["execution_id"],cursor=live["initial_cursor"],view="summary")
            waited = c.tool("wait_execution",execution_id=live["execution_id"],cursor=live["initial_cursor"],
                            state_cursor=observation["state_cursor"],view="summary",max_wait_ms=30)
            assert waited["timed_out"] and waited["detail_cursor"] == live["initial_cursor"]
            assert "unread" in json.dumps(c.tool("read_execution",execution_id=live["execution_id"],cursor=live["initial_cursor"]))
            finish(c, live)
            # Bounded history: a summary must report loss without acknowledging the lost range.
            flood = c.tool("start_execution",session_id=session,command=[sys.executable,"-c","import sys;sys.stdout.write('z'*1200000)"],
                           profile="host",io="pipes")
            finish(c, flood)
            gap = c.tool("read_execution",execution_id=flood["execution_id"],cursor=flood["initial_cursor"],view="summary")
            assert gap["catch_up_required"] and gap["detail_cursor"] == flood["initial_cursor"]
            assert len(json.dumps(gap)) < 4096
            # Kill only the temporary broker while a short child is alive. No stop/restart is inferred.
            unknown_args = dict(session_id=session,command=["/bin/sleep","1"],profile="host",io="pipes",idempotency_key="unknown")
            unknown = c.tool("start_execution",**unknown_args)
            c.close()
            broker.kill()
            broker.wait(timeout=5)
            broker = boot()
            c = frontend()
            archived = c.tool("read_execution",execution_id=job_id,cursor=initial)
            assert archived["execution"]["status"] == "exited"
            assert c.tool("start_execution",**args)["execution_id"] == job_id
            assert counter.read_text() == "once\n"
            resumed = c.tool("start_execution",**unknown_args)
            assert resumed["execution_id"] == unknown["execution_id"] and resumed["status"] == "outcome_unknown"
            recovered = c.tool("list_sessions")
            assert len(recovered["checkpoints"]) == 2
            assert any(v["execution_id"] == unknown["execution_id"] for v in recovered["executions"])
            assert (state / "recovery.json").stat().st_size < 4 * 1024 * 1024
            # Continue beyond three retention windows, with both keyed and unkeyed starts.
            retired_counter = project / "retired-starts"
            legacy_args = dict(session_id=session,command=["/bin/sh","-c",f"echo legacy >> {retired_counter}"],profile="host",io="pipes",idempotency_key="retire-legacy")
            retired_legacy = c.tool("start_execution",**legacy_args)
            finish(c, retired_legacy)
            old_generation = c.tool("list_sessions")["recovery"]["key_generation"]
            retired_args = dict(session_id=session,command=["/bin/sh","-c",f"echo keyed >> {retired_counter}"],profile="host",io="pipes",idempotency_key="retire-explicit",key_generation=old_generation)
            retired_job = c.tool("start_execution",**retired_args)
            finish(c, retired_job)
            for n in range(400):
                extra = {} if n % 2 else dict(idempotency_key=f"continued-{n}",key_generation=c.tool("list_sessions")["recovery"]["key_generation"])
                short = c.tool("start_execution",session_id=session,command=["/bin/true"],profile="host",io="pipes",**extra)
                finish(c, short)
            retained = c.tool("list_sessions")
            assert len(retained["executions"]) <= 128 and retained["recovery"]["retired_records"] > 256
            assert retained["recovery"]["key_generation"] != old_generation
            assert any(v["execution_id"] == unknown["execution_id"] for v in retained["executions"])
            assert len(retained["checkpoints"]) == 2
            assert c.tool("start_execution",**args)["execution_id"] == job_id  # unfinished readers pin this work
            for old_args in (legacy_args,retired_args):
                rejected = c.request("tools/call",{"name":"start_execution","arguments":old_args})
                assert rejected["result"]["isError"]
            assert retired_counter.read_text() == "legacy\nkeyed\n"
            assert counter.read_text() == "once\n"
            c.close()
            broker.terminate(); broker.wait(timeout=8)
            broker = boot(); c = frontend()
            for old_args in (legacy_args,retired_args):
                rejected = c.request("tools/call",{"name":"start_execution","arguments":old_args})
                assert rejected["result"]["isError"]
            assert retired_counter.read_text() == "legacy\nkeyed\n"
            assert c.tool("start_execution",**unknown_args)["status"] == "outcome_unknown"
            fresh = c.tool("start_execution",session_id=session,command=["/bin/true"],profile="host",io="pipes",
                           idempotency_key="after-retirement-restart",key_generation=c.tool("list_sessions")["recovery"]["key_generation"])
            finish(c,fresh)
            assert (state / "recovery.json").stat().st_size < 4 * 1024 * 1024
            print(f"PASS: start/input/checkpoint ACK loss; job.cursor recovery; two readers; Resources; restart/unknown; 400 starts; retirement/expired keys; read-only timeout retries={read_retries}")
        finally:
            for client in clients:
                client.cleanup()
            for process in processes:
                if process.poll() is None:
                    process.terminate()
                    try: process.wait(timeout=8)
                    except subprocess.TimeoutExpired:
                        process.kill(); process.wait()
                process.stderr.close()
            # The crash fixture's only orphan is a one-second sleep, never production.
            time.sleep(1.1)

def input_storage_failure(binary):
    with tempfile.TemporaryDirectory(prefix="dev-session-input-fault-") as directory:
        root = Path(directory); state = root / "state"; received = root / "received"
        env = {"PATH":os.defpath,"LANG":"C.UTF-8","DEV_SESSION_MCP_STATE_DIR":str(state),
               "DEV_SESSION_MCP_BROKER_SOCKET":str(state / "broker.sock")}
        process = subprocess.Popen([binary,"broker"],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
        client = None
        try:
            deadline = time.monotonic()+5
            while not (state / "broker.sock").exists():
                assert process.poll() is None and time.monotonic() < deadline
                time.sleep(.02)
            client = RecoveryFrontend(binary,env)
            session = client.tool("open_session",cwd=str(root))["id"]
            job = client.tool("start_execution",session_id=session,command=["/bin/sh","-c",f"exec cat > {received}"],profile="host",io="pipes")
            # Fail the real temporary-file open in this isolated broker; never touch production state.
            (state / "recovery.tmp").mkdir()
            failed = client.request("tools/call",{"name":"input_execution","arguments":dict(execution_id=job["execution_id"],text="MUST_NOT_SEND",close_stdin=True)})
            assert failed["result"]["isError"] and "stdin not sent" in failed["result"]["content"][0]["text"]
            time.sleep(.1)
            assert not received.exists() or received.read_bytes() == b""
            record = client.tool("read_execution",execution_id=job["execution_id"])
            assert record["execution"]["input"]["delivery"] == "not_sent" and not record["persistence_ok"]
            client.tool("signal_execution",execution_id=job["execution_id"],signal="TERM")
            print("PASS: real stdin remains unsent when receipt persistence fails")
        finally:
            if client: client.cleanup()
            process.terminate()
            try: process.wait(timeout=8)
            except subprocess.TimeoutExpired: process.kill(); process.wait()
            process.stderr.close()

def old_backend_guard(binary, old_binary):
    with tempfile.TemporaryDirectory(prefix="dev-session-old-backend-") as directory:
        root = Path(directory)
        state = root / "state"
        env = {"PATH":os.defpath,"LANG":"C.UTF-8","DEV_SESSION_MCP_STATE_DIR":str(state),
               "DEV_SESSION_MCP_BROKER_SOCKET":str(state / "broker.sock")}
        process = subprocess.Popen([old_binary,"broker"],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
        client = None
        try:
            deadline = time.monotonic() + 5
            while not (state / "broker.sock").exists():
                assert process.poll() is None and time.monotonic() < deadline
                time.sleep(.02)
            assert broker_request(state,op="ping").get("recovery") is None
            client = RecoveryFrontend(binary,env)
            session = client.tool("open_session",cwd=str(root))["id"]
            target = root / "must-not-start"
            response = client.request("tools/call",{"name":"start_execution","arguments":dict(session_id=session,
                command=["/bin/touch",str(target)],profile="host",io="pipes",idempotency_key="unsupported")})
            assert response["result"]["isError"] and not target.exists()
            assert client.tool("list_sessions")["executions"] == []
            print("PASS: old broker rejects recovery-dependent start before side effects")
        finally:
            if client: client.cleanup()
            process.terminate()
            try: process.wait(timeout=5)
            except subprocess.TimeoutExpired: process.kill(); process.wait()
            process.stderr.close()

if __name__ == "__main__":
    binary = str(Path(sys.argv[1]).resolve())
    run(binary)
    input_storage_failure(binary)
    if len(sys.argv) > 2:
        old_backend_guard(binary,str(Path(sys.argv[2]).resolve()))

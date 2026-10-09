#!/usr/bin/env python3
"""Operator argv contract, isolated tempstate. Fake bwrap tests plumbing, NOT isolation."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from urllib.parse import quote
from recovery_stdio import RecoveryFrontend
from delivery_diagnostics_stdio import broker_request


def run(binary, old=None):
    with tempfile.TemporaryDirectory(prefix="dev-session-admin-") as directory:
        root = Path(directory)
        project = root / "project"
        project.mkdir()
        state = root / "state"
        config = root / "profiles.json"
        capture = root / "argv.json"
        fake = root / "fake-bwrap"
        fake.write_text("#!/usr/bin/python3\nimport json,os,sys\nfrom pathlib import Path\n"
                        f"Path({str(capture)!r}).write_text(json.dumps(sys.argv[1:]))\n"
                        "args=sys.argv[1:]; i=args.index('--'); os.execvp(args[i+1],args[i+1:])\n")
        fake.chmod(0o700)
        env = {"PATH": os.defpath, "LANG": "C.UTF-8", "DEV_SESSION_MCP_STATE_DIR": str(state),
               "DEV_SESSION_MCP_BROKER_SOCKET": str(state / "broker.sock"),
               "DEV_SESSION_MCP_PROFILES": str(config)}
        processes = []
        clients = []

        def publish(version="one", program=fake, args=None):
            data = {"version": 1, "profiles": [{"id": "test", "bwrap": str(program),
                    "args": args if args is not None else ["--ro-bind", "/", "/", "--setenv", "CANARY", "PRIVATE_" + version,
                            "{{writable_roots}}", "--chdir", "{{cwd}}"]}]}
            next_file = root / "next.json"
            next_file.write_text(json.dumps(data))
            next_file.chmod(0o600)
            next_file.replace(config)

        def boot(executable=binary):
            p = subprocess.Popen([executable, "broker"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
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
            raise AssertionError("fixture broker timeout")

        def frontend():
            c = RecoveryFrontend(binary, env)
            clients.append(c)
            c.request("server/discover")
            return c

        def rejected(c, **args):
            result = c.request("tools/call", {"name": "start_execution", "arguments": args})
            assert result.get("error") or result.get("result", {}).get("isError"), result
            assert "PRIVATE_" not in json.dumps(result)
            return result

        def finish(c, job):
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                page = c.tool("read_execution", execution_id=job["execution_id"], cursor=job["cursor"])
                if page["execution"]["status"] == "exited":
                    return page
                time.sleep(.03)
            raise AssertionError("fixture child timeout")

        def observations(c, job, policy):
            listed = next(v for v in c.tool("list_sessions")["executions"] if v["execution_id"] == job["execution_id"])
            assert listed["execution_policy"] == policy
            for name in ("read_execution", "wait_execution"):
                for view in ("summary", "detail"):
                    extra = {"max_wait_ms": 0} if name == "wait_execution" else {}
                    page = c.tool(name, execution_id=job["execution_id"], cursor=job["cursor"], view=view, **extra)
                    assert page["execution"]["execution_policy"] == policy
                    assert "PRIVATE_" not in json.dumps(page)
            uri = f"dev-session:///executions/{job['execution_id']}?cursor={quote(job['cursor'],safe='')}"
            resource = c.request("resources/read", {"uri": uri})["result"]
            assert json.loads(resource["contents"][0]["text"])["execution"]["execution_policy"] == policy
            assert "PRIVATE_" not in json.dumps(resource)

        try:
            publish()
            broker = boot()
            c = frontend()
            sid = c.tool("open_session", cwd=str(project))["id"]
            catalog = c.tool("list_sessions")["execution_profiles"]
            assert catalog["available"] and catalog["profiles"][0]["id"] == "admin:test"
            assert "PRIVATE_" not in json.dumps(catalog)
            counter = project / "counter"
            args = dict(session_id=sid, command=["/bin/sh", "-c", f"echo once >> '{counter}'; sleep .3; printf done"],
                        profile="admin:test", io="pipes", idempotency_key="same-intent")
            # Discard the ACK logically, recover using exactly the same request.
            c.tool("start_execution", **args)
            job = c.tool("start_execution", **args)
            assert job["replayed"]
            original = job["execution_policy"]
            deadline = time.monotonic() + 5
            while not capture.exists():
                assert time.monotonic() < deadline
                time.sleep(.01)
            captured = json.loads(capture.read_text())
            assert "PRIVATE_one" in captured and str(project) in captured
            assert captured[captured.index("--") + 1:] == args["command"]
            assert "--apply-seccomp-then-exec" not in captured
            publish("two")  # old execution stays bound to its old policy
            assert c.tool("list_sessions")["execution_profiles"]["profiles"][0]["definition_sha256"] != original["definition_sha256"]
            end = finish(c, job)
            assert end["execution"]["exit_code"] == 0
            assert "done" in json.dumps(end["events"])
            assert counter.read_text() == "once\n"
            c.tool("checkpoint_execution", execution_id=job["execution_id"], reader_id="review",
                   cursor=end["cursor"], expected_revision=0, purpose="profile fixture", completion_condition="done observed")
            observations(c, job, original)
            assert c.tool("start_execution", **args)["execution_policy"] == original
            rejected(c, **dict(args, profile="host"))
            fresh_args = dict(args, idempotency_key="new-intent", command=["/bin/true"])
            fresh = c.tool("start_execution", **fresh_args)
            assert fresh["execution_policy"] != original
            finish(c, fresh)
            for extra in ({"io": "pty"}, {"bwrap_args": []}, {"network": "host"}, {"config_file": str(config)}):
                rejected(c, **dict(fresh_args, idempotency_key="invalid", **extra))
            # Whole-file validation, privacy, no fallback; builtins remain usable.
            config.write_text('{"PRIVATE_PARSE_CANARY":')
            assert not c.tool("list_sessions")["execution_profiles"]["available"]
            rejected(c, **dict(fresh_args, idempotency_key="invalid-config"))
            assert c.tool("start_execution", **args)["execution_id"] == job["execution_id"]
            host = c.tool("start_execution", session_id=sid, command=["/bin/true"], profile="host", io="pipes")
            assert finish(c, host)["execution"]["exit_code"] == 0
            publish(args=["--args", "0"])
            rejected(c, **dict(fresh_args, idempotency_key="fd"))
            publish()
            config.chmod(0o666)
            rejected(c, **dict(fresh_args, idempotency_key="writable-config"))
            config.chmod(0o600)
            backup = root / "backup.json"
            config.rename(backup)
            config.symlink_to(backup)
            rejected(c, **dict(fresh_args, idempotency_key="symlink-config"))
            config.unlink()
            backup.rename(config)
            publish(program=root / "missing-bwrap")
            rejected(c, **dict(fresh_args, idempotency_key="missing-executable"))
            publish("before-restart")
            unfinished_args = dict(fresh_args, idempotency_key="unfinished", command=["/bin/sleep", "3"])
            unfinished = c.tool("start_execution", **unfinished_args)
            # Restart with a changed definition: old intent remains read-only replay.
            c.close()
            # Crash only this temporary fixture broker. Its short /bin/sleep child
            # has no side effects and exits on its own; saved running state is unknown.
            broker.kill()
            broker.wait(timeout=8)
            publish("three")
            broker = boot()
            c = frontend()
            replay = c.tool("start_execution", **args)
            assert replay["execution_id"] == job["execution_id"] and replay["execution_policy"] == original
            assert counter.read_text() == "once\n"
            observations(c, job, original)
            unknown = c.tool("start_execution", **unfinished_args)
            assert unknown["execution_id"] == unfinished["execution_id"]
            assert unknown["execution_policy"] == unfinished["execution_policy"]
            assert unknown["status"] == "outcome_unknown"
            # Actual standard sandbox probe is negative on this VPS. No payload rerun.
            sentinel = project / "sandbox-payload"
            negative = c.tool("start_execution", session_id=sid, command=["/bin/sh", "-c", f"touch '{sentinel}'"], profile="sandbox", io="pipes")
            result = finish(c, negative)
            if result["execution"]["exit_code"] != 0:
                assert not sentinel.exists()
                print("standard sandbox: NEGATIVE (setup failed; no host fallback), not an isolation success")
            else:
                print("standard sandbox: payload ran; this fixture does not prove isolation")
            # Deterministic failed launcher proves there is no automatic host retry,
            # independently of whether the machine permits real bubblewrap.
            failure_marker = project / "must-not-run"
            publish(program=Path("/bin/false"), args=[])
            failed = c.tool("start_execution", session_id=sid,
                            command=["/bin/sh", "-c", f"touch '{failure_marker}'"], profile="admin:test", io="pipes")
            assert finish(c, failed)["execution"]["exit_code"] != 0
            assert not failure_marker.exists()
            # Actual bwrap custom failure also never falls back.
            publish(program=Path("/usr/bin/bwrap"), args=["--ro-bind", "/", "/", "--unshare-net"])
            custom_negative = c.tool("start_execution", session_id=sid, command=["/bin/true"], profile="admin:test", io="pipes")
            actual = finish(c, custom_negative)
            print("real custom bwrap exit:", actual["execution"]["exit_code"])
            if old:
                c.close()
                broker_request(state, op="shutdown")
                broker.wait(timeout=8)
                broker = boot(old)
                c = frontend()
                blocked = rejected(c, **dict(fresh_args, idempotency_key="old-broker"))
                assert "broker lacks admin profiles" in json.dumps(blocked)
            print("admin profiles: argv/replay/edit/restart/redaction/CAS observations/invalid config/old broker contracts passed")
        finally:
            for c in clients:
                c.close()
            for p in processes:
                if p.poll() is None:
                    p.terminate()
                    try:
                        p.wait(timeout=8)
                    except subprocess.TimeoutExpired:
                        p.kill()
                        p.wait(timeout=5)


if __name__ == "__main__":
    run(str(Path(sys.argv[1]).resolve()), str(Path(sys.argv[2]).resolve()) if len(sys.argv) > 2 else None)

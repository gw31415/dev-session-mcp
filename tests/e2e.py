#!/usr/bin/env python3
"""End-to-end check of the real binary: stdio and Streamable HTTP frontends
connected at the same time, both MCP lifecycles, sandbox/host executions and
reconnect. Usage: python3 tests/e2e.py rust/target/debug/dev-session-mcp"""
import base64, json, os, pathlib, select, socket, subprocess, sys, tempfile, time, urllib.error, urllib.request

binary = str(pathlib.Path(sys.argv[1]).resolve())
META = {"io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "e2e", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {}}
TOOLS = {'open_session', 'list_sessions', 'close_session', 'start_execution', 'input_execution',
         'resize_execution', 'signal_execution', 'read_execution', 'wait_execution', 'checkpoint_execution',
         'read_file', 'write_file', 'list_directory', 'get_image', 'import_file'}


def environment(state):
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("MCP_", "TUNNEL_", "CONTROL_PLANE_")) and k not in ("OPENAI_ADMIN_KEY", "CREDENTIALS_DIRECTORY")}
    env['DEV_SESSION_MCP_STATE_DIR'] = str(state)
    env['TMPDIR'] = str(state.parent / 'tmp')
    pathlib.Path(env['TMPDIR']).mkdir(exist_ok=True)
    return env


class Client:
    """Shared tool helpers; subclasses implement request()."""
    def tool(self, name, **args):
        r = self.request('tools/call', {'name': name, 'arguments': args})
        assert 'error' not in r, r
        assert not r['result'].get('isError'), r['result']
        return r['result']['structuredContent']

    def bad_tool(self, name, **args):
        r = self.request('tools/call', {'name': name, 'arguments': args})
        assert 'error' in r or r['result'].get('isError'), r


class Stdio(Client):
    def __init__(self, state, legacy=False):
        self.p = subprocess.Popen([binary, 'stdio'], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, env=environment(state))
        self.n, self.legacy = 0, legacy
        if legacy:
            init = self.request('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                                               'clientInfo': {'name': 'e2e-legacy', 'version': '1'}})
            assert init['result']['protocolVersion'] == '2025-06-18', init
            self.send({'jsonrpc': '2.0', 'method': 'notifications/initialized'})

    def send(self, message):
        self.p.stdin.write((json.dumps(message) + '\n').encode())
        self.p.stdin.flush()

    def request(self, method, params=None):
        self.n += 1
        params = dict(params or {})
        if not self.legacy:
            params['_meta'] = META
        self.send({'jsonrpc': '2.0', 'id': self.n, 'method': method, 'params': params})
        assert select.select([self.p.stdout], [], [], 20)[0], 'MCP response timeout'
        line = self.p.stdout.readline()
        assert line, self.p.stderr.read().decode()
        response = json.loads(line)
        assert response['id'] == self.n, response
        return response

    def close(self):
        self.p.stdin.close()
        self.p.wait(timeout=10)
        assert self.p.returncode == 0, self.p.stderr.read().decode()


class Http(Client):
    def __init__(self, url, version):
        self.url, self.version, self.n = url, version, 0

    def post(self, body, headers=None):
        request = urllib.request.Request(self.url, data=json.dumps(body).encode(), method='POST', headers={
            'Content-Type': 'application/json', 'Accept': 'application/json, text/event-stream',
            'MCP-Protocol-Version': self.version, **(headers or {})})
        try:
            with urllib.request.urlopen(request, timeout=20) as response:
                return response.status, response.read().decode()
        except urllib.error.HTTPError as error:
            return error.code, error.read().decode()

    def request(self, method, params=None):
        self.n += 1
        params = dict(params or {})
        if self.version == '2026-07-28':
            params['_meta'] = META
        headers = {}
        if self.version == '2026-07-28':  # SEP-2243 routing headers
            headers['Mcp-Method'] = method
            if method == 'tools/call':
                headers['Mcp-Name'] = params['name']
        status, text = self.post({'jsonrpc': '2.0', 'id': self.n, 'method': method, 'params': params}, headers)
        assert status == 200, (status, text)
        if text.lstrip().startswith('{'):
            return json.loads(text)
        data = [l[5:].strip() for l in text.splitlines() if l.startswith('data:') and l[5:].strip()]
        return json.loads(data[-1])


def broker(state, op):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(5)
        s.connect(str(state / 'broker.sock'))
        s.sendall((json.dumps({'op': op}) + '\n').encode())
        return json.loads(s.makefile('rb').readline())


def wait_exit(m, execution, cursor):
    events = []
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        r = m.tool('wait_execution', execution_id=execution, cursor=cursor, max_wait_ms=2000)
        events.extend(r['events'])
        cursor = r['cursor']
        if r['execution']['status'] == 'exited' and not r['more']:
            return r, events
    raise AssertionError('execution did not exit')


def text(events, stream=None):
    return ''.join(e['data'].get('text', '') for e in events
                   if e['kind'] == 'output' and (stream is None or e['data']['stream'] == stream))


def main():
    with tempfile.TemporaryDirectory(prefix='dev-session-mcp-e2e-') as directory:
        root = pathlib.Path(directory)
        state, project = root / 'state', root / 'project'
        project.mkdir()
        port = socket.create_server(('127.0.0.1', 0)).getsockname()[1]  # closed immediately; reused below
        a = Stdio(state)
        b = Stdio(state, legacy=True)  # concurrent second stdio frontend, 2025 lifecycle
        http = subprocess.Popen([binary, 'http', '--listen', f'127.0.0.1:{port}', '--allowed-host', 'mcp.example.test'],
                                stderr=subprocess.PIPE, env=environment(state))
        try:
            for _ in range(200):
                try:
                    urllib.request.urlopen(f'http://127.0.0.1:{port}/healthz', timeout=1).read()
                    break
                except OSError:
                    time.sleep(.05)
            else:
                raise AssertionError('http frontend did not start')
            h = Http(f'http://127.0.0.1:{port}/mcp', '2026-07-28')
            legacy_http = Http(f'http://127.0.0.1:{port}/mcp', '2025-06-18')

            # Discovery on both transports and both lifecycles.
            assert a.request('server/discover')['result']['capabilities']['events'] == {}
            assert h.request('server/discover')['result']['capabilities']['events'] == {}
            init = legacy_http.request('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                                                      'clientInfo': {'name': 'e2e-http', 'version': '1'}})
            assert init['result']['protocolVersion'] == '2025-06-18', init
            for client in (a, b, h, legacy_http):
                assert {t['name'] for t in client.request('tools/list')['result']['tools']} == TOOLS

            # The HTTP frontend has no authentication: it must reject foreign Host and browser Origin values.
            status, _ = h.post({'jsonrpc': '2.0', 'id': 99, 'method': 'tools/list', 'params': {'_meta': META}},
                               {'Host': 'attacker.example', 'Mcp-Method': 'tools/list'})
            assert status == 403, status
            status, _ = h.post({'jsonrpc': '2.0', 'id': 99, 'method': 'tools/list', 'params': {'_meta': META}},
                               {'Origin': 'https://attacker.example', 'Mcp-Method': 'tools/list'})
            assert status == 403, status
            status, _ = h.post({'jsonrpc': '2.0', 'id': 99, 'method': 'tools/list', 'params': {'_meta': META}},
                               {'Host': 'mcp.example.test', 'Mcp-Method': 'tools/list'})
            assert status == 200, status

            # One broker: a session opened over stdio is the same session over HTTP.
            session = a.tool('open_session', cwd=str(project))['id']
            assert h.tool('open_session', cwd=str(project))['id'] == session == b.tool('open_session', cwd=str(project))['id']

            # Sandbox file writes stay inside session roots.
            h.tool('write_file', session_id=session, path='note.txt', content='日本語\n')
            assert a.tool('read_file', session_id=session, path='note.txt')['text'] == '日本語\n'
            h.bad_tool('write_file', session_id=session, path=str(root / 'outside.txt'), content='blocked')
            assert not (root / 'outside.txt').exists()

            # Pipes keep stdout/stderr apart; a sandbox execution runs without network.
            pipe = h.tool('start_execution', session_id=session, command=['/bin/sh', '-c', 'printf out; printf err >&2'],
                          profile='host', io='pipes')
            _, events = wait_exit(b, pipe['execution_id'], pipe['cursor'])
            assert text(events, 'stdout') == 'out' and text(events, 'stderr') == 'err'
            sandbox = a.tool('start_execution', session_id=session, command=['/bin/sh', '-c', 'printf sandbox'],
                             profile='sandbox', io='pipes')
            r, events = wait_exit(h, sandbox['execution_id'], sandbox['cursor'])
            assert r['execution']['exit_code'] == 0 and text(events) == 'sandbox'
            a.bad_tool('start_execution', session_id=session, command=['/bin/true'], profile='sandbox', io='pipes', cwd=str(root))

            # PTY input/resize from one frontend, observed from another after the first reconnects.
            pty = h.tool('start_execution', session_id=session, profile='host',
                         command=['/bin/sh', '-c', 'stty -echo; printf READY; read x; printf "GOT:%s" "$x"; sleep 30'])
            time.sleep(.2)
            assert a.tool('input_execution', execution_id=pty['execution_id'], text='hello\n')['input']['delivery'] == 'accepted'
            h.tool('resize_execution', execution_id=pty['execution_id'], rows=33, cols=90)
            epoch = broker(state, 'ping')['result']['epoch']
            a.close()
            a = Stdio(state)
            assert broker(state, 'ping')['result']['epoch'] == epoch
            deadline = time.monotonic() + 10
            seen = ''
            while 'GOT:hello' not in seen and time.monotonic() < deadline:
                seen = text(a.tool('read_execution', execution_id=pty['execution_id'], cursor=pty['cursor'])['events'])
                time.sleep(.05)
            assert 'GOT:hello' in seen, seen
            b.tool('signal_execution', execution_id=pty['execution_id'], signal='TERM')
            wait_exit(h, pty['execution_id'], pty['cursor'])

            # Pipe EOF.
            eof = h.tool('start_execution', session_id=session, command=['/usr/bin/sha256sum'], profile='host', io='pipes')
            h.tool('input_execution', execution_id=eof['execution_id'], text='EOF bytes', close_stdin=True)
            r, events = wait_exit(h, eof['execution_id'], eof['cursor'])
            assert __import__('hashlib').sha256(b'EOF bytes').hexdigest() in text(events)

            # Events: listed everywhere, subscriptions owned by the broker reject private callbacks.
            assert h.request('events/list')['result']['events'][0]['delivery'] == ['webhook']
            secret = 'whsec_' + base64.b64encode(b'x' * 32).decode()
            for client in (a, h):
                rejected = client.request('events/subscribe', {'name': 'execution.events', 'arguments': {'session_id': session},
                                                               'delivery': {'mode': 'webhook', 'url': 'https://127.0.0.1/', 'secret': secret}})
                assert rejected['error']['code'] == -32015 and 'whsec_' not in json.dumps(rejected), rejected

            # Closing confirms exit and is visible to every frontend.
            alive = a.tool('start_execution', session_id=session, command=['/bin/sleep', '30'], profile='host', io='pipes')
            closed = h.tool('close_session', session_id=session)
            assert closed['closed'] and not closed['pending']
            r, events = wait_exit(b, alive['execution_id'], alive['cursor'])
            kinds = [e['kind'] for e in events]
            assert kinds.index('closing') < kinds.index('exit') < kinds.index('closed')
            assert a.tool('list_sessions')['sessions'] == []

            # A long-running HTTP frontend restarts a broker that went away.
            broker(state, 'shutdown')
            for _ in range(200):
                if not (state / 'broker.sock').exists():
                    break
                time.sleep(.02)
            h.bad_tool('signal_execution', execution_id=alive['execution_id'], signal='KILL')
            assert h.tool('open_session', cwd=str(project))['id']
            assert broker(state, 'ping')['result']['epoch'] != epoch
            print('PASS: stdio (2026 + 2025 lifecycles) and Streamable HTTP frontends concurrently on one broker; '
                  'Host/Origin checks, sandbox files and executions, pipes, PTY input/resize/reconnect, EOF, '
                  'broker-owned Events, confirmed close, broker restart')
        finally:
            for client in (a, b):
                if client.p.poll() is None:
                    client.close()
            http.terminate()
            http.wait(timeout=10)
            if (state / 'broker.sock').exists():
                broker(state, 'shutdown')


if __name__ == '__main__':
    main()

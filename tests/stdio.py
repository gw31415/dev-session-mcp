#!/usr/bin/env python3
"""Real stdio, PTY, pipe, sandbox, bounded journal and reconnect checks."""
import json, os, pathlib, select, socket, subprocess, sys, tempfile, time

binary = str(pathlib.Path(sys.argv[1]).resolve())
meta = {"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "local-check", "version": "1"}, "io.modelcontextprotocol/clientCapabilities": {}}

class MCP:
    def __init__(self, state):
        env = {k: v for k, v in os.environ.items() if not k.startswith(("MCP_", "TUNNEL_", "CONTROL_PLANE_")) and k not in ("OPENAI_ADMIN_KEY", "CREDENTIALS_DIRECTORY")}
        env['DEV_SESSION_MCP_STATE_DIR'] = str(state)
        env['TMPDIR'] = str(state.parent/'tmp'); pathlib.Path(env['TMPDIR']).mkdir(exist_ok=True)
        self.p = subprocess.Popen([binary, 'stdio'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
        self.n = 0
    def request(self, method, params=None):
        self.n += 1
        params = dict(params or {}, _meta=meta)
        self.p.stdin.write((json.dumps(dict(jsonrpc='2.0', id=self.n, method=method, params=params))+'\n').encode()); self.p.stdin.flush()
        assert select.select([self.p.stdout], [], [], 15)[0], 'MCP response timeout'
        line = self.p.stdout.readline()
        assert line, self.p.stderr.read().decode()
        response = json.loads(line)
        assert response['id'] == self.n, response
        return response
    def tool(self, name, **args):
        r = self.request('tools/call', {'name': name, 'arguments': args})
        assert 'error' not in r, r
        result = r['result']
        assert not result.get('isError'), result
        return result['structuredContent']
    def bad_tool(self, name, **args):
        r = self.request('tools/call', {'name': name, 'arguments': args})
        assert 'error' in r or r.get('result', {}).get('isError'), r
    def close(self):
        self.p.stdin.close(); self.p.wait(timeout=10)
        assert self.p.returncode == 0, self.p.stderr.read().decode()

def broker(state, op):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(5); s.connect(str(state/'broker.sock'))
        s.sendall((json.dumps({'op': op})+'\n').encode())
        return json.loads(s.makefile('rb').readline())

def wait_exit(m, execution, cursor):
    events = []
    deadline = time.monotonic()+15
    while time.monotonic() < deadline:
        r = m.tool('read_execution', execution_id=execution, cursor=cursor)
        events.extend(r['events']); cursor = r['cursor']
        if r['execution']['status'] == 'exited' and not r['more']: return r, events
        time.sleep(.03)
    raise AssertionError('execution did not exit')

with tempfile.TemporaryDirectory(prefix='dev-session-mcp-check-') as directory:
    root = pathlib.Path(directory); state = root/'state'; project = root/'project'; project.mkdir()
    m = MCP(state)
    try:
        discovered = m.request('server/discover')['result']
        assert discovered['capabilities']['events'] == {}, discovered
        names = {t['name'] for t in m.request('tools/list')['result']['tools']}
        assert names == {'open_session','list_sessions','close_session','start_execution','input_execution','resize_execution','signal_execution','read_execution','read_file','write_file','list_directory','get_image','import_file'}, names
        session = m.tool('open_session', cwd=str(project))['id']
        assert m.tool('open_session', cwd=str(project))['id'] == session
        assert m.tool('list_sessions')['executions'] == []
        m.tool('write_file', session_id=session, path='note.txt', content='日本語\nintent\n')
        assert m.tool('read_file', session_id=session, path='note.txt')['text'] == '日本語\nintent\n'
        m.bad_tool('write_file', session_id=session, path=str(root/'outside.txt'), content='blocked')
        assert not (root/'outside.txt').exists()
        (project/'link').symlink_to(root/'outside.txt')
        m.bad_tool('write_file', session_id=session, path='link', content='blocked')
        assert not (root/'outside.txt').exists()
        (project/'large.txt').write_text('あ'*30000)
        file = m.tool('read_file', session_id=session, path='large.txt'); assert file['truncated'] and len(file['text'].encode()) <= 65538
        pipe = m.tool('start_execution', session_id=session, command=['/bin/sh','-c','printf out; printf err >&2; sleep .15'], profile='host', io='pipes')
        r, events = wait_exit(m, pipe['execution_id'], pipe['cursor'])
        assert ''.join(e['data']['text'] for e in events if e['kind']=='output' and e['data']['stream']=='stdout') == 'out'
        assert ''.join(e['data']['text'] for e in events if e['kind']=='output' and e['data']['stream']=='stderr') == 'err'
        assert m.tool('read_execution', execution_id=pipe['execution_id'], cursor=r['cursor'])['events'] == []
        pty = m.tool('start_execution', session_id=session, command=['/bin/sh','-c','stty -echo; printf READY; read x; printf "GOT:%s" "$x"; sleep 30'], profile='host')
        other = m.tool('start_execution', session_id=session, command=['/bin/sh','-c','sleep .2; printf other'], profile='host', io='pipes')
        time.sleep(.2)
        receipt = m.tool('input_execution', execution_id=pty['execution_id'], text='hello\n')['input']
        assert receipt['delivery'] == 'accepted'
        m.tool('resize_execution', execution_id=pty['execution_id'], rows=33, cols=90)
        bare = str(pty['pid']); m.bad_tool('signal_execution', execution_id=bare, signal='KILL')
        stale = pty['execution_id'].rsplit(':',1)[0]+':0'; m.bad_tool('signal_execution', execution_id=stale, signal='KILL')
        # Restart only the frontend; the independent broker keeps this exact PID.
        epoch = broker(state,'ping')['result']['epoch']; m.close(); m = MCP(state)
        assert broker(state,'ping')['result']['epoch'] == epoch
        restored = m.tool('read_execution', execution_id=pty['execution_id'], cursor=pty['cursor'])
        assert restored['execution']['pid'] == pty['pid'] and restored['execution']['status'] == 'running'
        assert any(e['kind']=='input' and e['data'].get('delivery')=='written' for e in restored['events'])
        assert 'GOT:hello' in ''.join(e['data'].get('text','') for e in restored['events'])
        assert any(e['kind']=='resize' and e['data']['applied'] for e in restored['events'])
        m.tool('signal_execution', execution_id=pty['execution_id'], signal='TERM')
        wait_exit(m, pty['execution_id'], restored['cursor'])
        wait_exit(m, other['execution_id'], other['cursor'])
        sandbox = m.tool('start_execution', session_id=session, command=['/bin/sh','-c','printf sandbox'], profile='sandbox', io='pipes')
        r, events = wait_exit(m,sandbox['execution_id'],sandbox['cursor']); assert r['execution']['exit_code']==0 and any(e['data'].get('text')=='sandbox' for e in events)
        flood = m.tool('start_execution', session_id=session, command=['/usr/bin/python3','-c','import sys;sys.stdout.write("\\x00あ"*600000)'], profile='host', io='pipes')
        time.sleep(.5)
        r, events = wait_exit(m,flood['execution_id'],flood['cursor'])
        assert r['execution']['exit_code']==0
        loss = m.tool('read_execution',execution_id=flood['execution_id'],cursor=flood['cursor'])
        assert loss['catch_up_required'] and len(json.dumps(loss['events'],ensure_ascii=False).encode()) < 33000
        uncertain = m.tool('start_execution', session_id=session, command=['/bin/sh','-c','exec 0<&-; printf CLOSED; sleep 30'], profile='host', io='pipes')
        for _ in range(100):
            ready = m.tool('read_execution', execution_id=uncertain['execution_id'], cursor=uncertain['cursor'])
            if any(e['data'].get('text') == 'CLOSED' for e in ready['events']): break
            time.sleep(.02)
        else: raise AssertionError('closed-stdin process not ready')
        m.tool('input_execution', execution_id=uncertain['execution_id'], text='once')
        for _ in range(100):
            receipt = m.tool('read_execution', execution_id=uncertain['execution_id'])['execution'].get('input', {})
            if receipt.get('delivery') == 'delivery_unknown': break
            time.sleep(.02)
        else: raise AssertionError('failed OS stdin write must remain uncertain')
        m.tool('signal_execution', execution_id=uncertain['execution_id'], signal='KILL')
        wait_exit(m,uncertain['execution_id'],uncertain['cursor'])
        events_list = m.request('events/list')['result']; assert events_list['events'][0]['delivery']==['webhook']
        rejected = m.request('events/subscribe', {'name':'execution.events','arguments':{'session_id':session},'delivery':{'mode':'webhook','url':'https://127.0.0.1/','secret':'whsec_'+__import__('base64').b64encode(b'x'*32).decode()}})
        assert rejected['error']['code']==-32015 and 'whsec_' not in json.dumps(rejected)
        alive = m.tool('start_execution', session_id=session,command=['/bin/sleep','30'],profile='host',io='pipes')
        m.tool('close_session',session_id=session)
        wait_exit(m,alive['execution_id'],alive['cursor'])
        assert m.tool('list_sessions')['sessions'] == []
        m.close(); broker(state,'shutdown')
        for _ in range(100):
            if not (state/'broker.sock').exists(): break
            time.sleep(.02)
        m = MCP(state)
        m.bad_tool('signal_execution',execution_id=alive['execution_id'],signal='KILL')
        session = m.tool('open_session',cwd=str(project))['id']
        fresh = m.tool('start_execution',session_id=session,command=['/bin/true'],profile='host',io='pipes')
        assert m.tool('read_execution',execution_id=fresh['execution_id'],cursor=alive['cursor'])['catch_up_required']
        wait_exit(m,fresh['execution_id'],fresh['cursor'])
        m.tool('close_session',session_id=session)
        print('PASS: stdio discovery/13 tools, sandbox files, separate pipes, PTY stdin/resize/signal, concurrency, frontend reconnect, PID/broker-epoch rejection, uncertain input receipt, bounded gap, private callback rejection, session close')
    finally:
        if m.p.poll() is None: m.close()
        if (state/'broker.sock').exists(): broker(state,'shutdown')

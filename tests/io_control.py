#!/usr/bin/env python3
"""Small real-process check for pipe EOF, control backpressure and close."""
import hashlib, json, os, pathlib, tempfile, time
from stdio import MCP, broker, wait_exit

with tempfile.TemporaryDirectory(prefix='dev-session-mcp-io-') as directory:
    root=pathlib.Path(directory);state=root/'state';project=root/'project';project.mkdir()
    m=MCP(state)
    try:
        assert m.request('server/discover')['result']['capabilities']['events']=={}
        session=m.tool('open_session',cwd=str(project))['id']
        m.tool('write_file',session_id=session,path='smoke.txt',content='ARM-safe smoke\n')
        assert m.tool('read_file',session_id=session,path='smoke.txt')['text']=='ARM-safe smoke\n'
        eof=m.tool('start_execution',session_id=session,command=['/usr/bin/sha256sum'],profile='host',io='pipes')
        m.tool('input_execution',execution_id=eof['execution_id'],text='EOF bytes',close_stdin=True)
        r,events=wait_exit(m,eof['execution_id'],eof['cursor'])
        assert r['execution']['exit_code']==0 and r['execution']['stdin_closed']
        assert hashlib.sha256(b'EOF bytes').hexdigest() in ''.join(e['data'].get('text','') for e in events)
        only_eof=m.tool('start_execution',session_id=session,command=['/bin/cat'],profile='host',io='pipes')
        m.tool('input_execution',execution_id=only_eof['execution_id'],close_stdin=True)
        assert wait_exit(m,only_eof['execution_id'],only_eof['cursor'])[0]['execution']['exit_code']==0
        pty=m.tool('start_execution',session_id=session,command=['/bin/sh','-c','stty -echo; printf READY; read line; printf "GOT:%s" "$line"; sleep 30'],profile='host')
        for _ in range(100):
            r=m.tool('read_execution',execution_id=pty['execution_id'],cursor=pty['cursor'])
            if 'READY' in ''.join(e['data'].get('text','') for e in r['events']):break
            time.sleep(.02)
        else:raise AssertionError('PTY did not become ready')
        m.bad_tool('input_execution',execution_id=pty['execution_id'],close_stdin=True)
        m.tool('input_execution',execution_id=pty['execution_id'],text='once\n')
        m.tool('resize_execution',execution_id=pty['execution_id'],rows=35,cols=90)
        m.close();m=MCP(state)
        for _ in range(100):
            r=m.tool('read_execution',execution_id=pty['execution_id'],cursor=pty['cursor'])
            if 'GOT:once' in ''.join(e['data'].get('text','') for e in r['events']):break
            time.sleep(.02)
        else:raise AssertionError('same PTY did not reconnect')
        assert r['execution']['pid']==pty['pid']
        m.tool('signal_execution',execution_id=pty['execution_id'],signal='TERM');wait_exit(m,pty['execution_id'],r['cursor'])
        blocked=m.tool('start_execution',session_id=session,command=['/bin/sleep','30'],profile='host',io='pipes')
        for _ in range(3):m.tool('input_execution',execution_id=blocked['execution_id'],text='x'*65536)
        started=time.monotonic();m.tool('signal_execution',execution_id=blocked['execution_id'],signal='TERM')
        wait_exit(m,blocked['execution_id'],blocked['cursor']);assert time.monotonic()-started<3
        closing=m.tool('start_execution',session_id=session,command=['/bin/sleep','30'],profile='host',io='pipes')
        for _ in range(3):m.tool('input_execution',execution_id=closing['execution_id'],text='x'*65536)
        started=time.monotonic();closed=m.tool('close_session',session_id=session)
        assert closed['closed'] and not closed['pending'] and time.monotonic()-started<3
        r,events=wait_exit(m,closing['execution_id'],closing['cursor'])
        kinds=[e['kind'] for e in events];assert kinds.index('closing')<kinds.index('exit')<kinds.index('closed')
        assert r['execution']['session_state']=='closed' and m.tool('list_sessions')['sessions']==[]
        try:os.kill(closing['pid'],0)
        except ProcessLookupError:pass
        else:raise AssertionError('closed reported before child reaping')
        assert m.tool('close_session',session_id=session)['closed']
        print(json.dumps({'result':'PASS','cases':['stdio Events discovery','sandbox file edit','pipe text+EOF','pipe EOF-only','PTY pipe-close rejection','stdin/resize/frontend reconnect','signal bypasses blocked stdin','confirmed close/ordered terminal history','idempotent close'],'temporary_state':str(state)}))
    finally:
        if m.p.poll() is None:m.close()
        if (state/'broker.sock').exists():broker(state,'shutdown')

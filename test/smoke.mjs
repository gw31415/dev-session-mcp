import assert from 'node:assert/strict';
import * as fs from 'node:fs/promises';
import path from 'node:path';
import { spawn, execFileSync } from 'node:child_process';
import { once } from 'node:events';
import { Client } from '@modelcontextprotocol/client';
import { StdioClientTransport } from '@modelcontextprotocol/client/stdio';

const root=path.resolve('.'),home=await fs.mkdtemp('/tmp/odm-smoke-'),cwd=path.join(home,'project');
await fs.mkdir(cwd);const state=path.join(home,'state');
const env={PATH:process.env.PATH,HOME:home,LANG:'C.UTF-8',XDG_STATE_HOME:path.join(home,'xdg'),OCI_DEV_STATE_DIR:state,OCI_DEV_LOCAL_MCP_BIN:path.resolve(process.env.OCI_DEV_LOCAL_MCP_BIN??'vendor/local-mcp/target/debug/local-mcp'),OCI_DEV_TMUX_BIN:process.env.OCI_DEV_TMUX_BIN??'tmux'};
let errors='';
function startDaemon(){const d=spawn(process.execPath,['src/daemon.mjs'],{cwd:root,env,stdio:['ignore','ignore','pipe']});d.stderr.on('data',b=>errors+=b.toString());return d;}
let daemon=startDaemon();
const pause=ms=>new Promise(r=>setTimeout(r,ms));let client;
async function connect() {const c=new Client({name:'real-smoke',version:'1'});await c.connect(new StdioClientTransport({command:process.execPath,args:[path.join(root,'src/server.mjs')],env,stderr:'pipe'}));return c;}
async function raw(name,args) {return client.callTool({name,arguments:args});}
async function call(name,args) {const result=await raw(name,args);assert(!result.isError,JSON.stringify(result));return JSON.parse(result.content[0].text);}
async function until(args,predicate) {for(let i=0;i<100;i++){const r=await call('mux_poll',args);if(predicate(r))return r;await pause(50);}throw new Error('terminal wait expired');}
try {
  for(let i=0;i<100;i++){try{await fs.stat(path.join(state,'backend.sock'));break;}catch{await pause(100);}}
  assert(daemon.exitCode===null,'daemon failed: '+errors);
  client=await connect();const list=await client.listTools();
  assert(list.tools.some(x=>x.name==='execute'));assert(list.tools.some(x=>x.name==='get_memo'));console.log('PASS real stdio initialize/tools/list: '+list.tools.length+' tools');
  const s=await call('create_session',{session_id:'smoke.test',cwd});assert.equal(s.cwd,cwd);
  assert.equal((await call('list_sessions',{})).sessions.length,1);
  assert.equal((await call('connect_session',{session_id:s.session_id})).cwd,cwd);console.log('PASS real upstream session creation/list/connect');
  await call('set_memo',{session_id:s.session_id,text:'Purpose: smoke\nNext: reconnect'});assert.match((await call('get_memo',{session_id:s.session_id})).text,/reconnect/);console.log('PASS explicit memo write/read');
  let r=await raw('execute',{session_id:s.session_id,command:['/bin/sh','-c','printf REAL_EXEC_OK']});assert(!r.isError,JSON.stringify(r));assert.match(r.content[0].text,/REAL_EXEC_OK/);console.log('PASS reused upstream execute');
  r=await raw('write_file',{session_id:s.session_id,path:'test.txt',content:'before\n'});assert(!r.isError,JSON.stringify(r));
  r=await raw('read_file',{session_id:s.session_id,path:'test.txt'});assert.equal(r.content[0].text,'before\n');
  r=await raw('write_file',{session_id:s.session_id,path:'test.txt',content:'after\n'});assert(!r.isError,JSON.stringify(r));assert.equal(await fs.readFile(path.join(cwd,'test.txt'),'utf8'),'after\n');console.log('PASS reused upstream read/write/edit');
  const upstreamJob=JSON.parse((await raw('start_command',{session_id:s.session_id,command:['/bin/sh','-c','sleep 2; printf UPSTREAM_RECONNECTED']})).content[0].text);
  assert((await call('connect_session',{session_id:s.session_id})).upstream_jobs.some(x=>x.job_id===upstreamJob.job_id));
  const j=await call('mux_open',{session_id:s.session_id,command:['/bin/bash','-c','read value; printf "STDIN:%s\\n" "$value"; sleep 30']});
  await client.close();client=await connect();assert.equal((await call('connect_session',{session_id:s.session_id})).session_id,s.session_id);assert.match((await call('get_memo',{session_id:s.session_id})).text,/reconnect/);
  await call('mux_send',{session_id:s.session_id,job_id:j.job_id,text:'hello',keys:['Enter']});
  const snapshot=await until({session_id:s.session_id,job_id:j.job_id},r=>r.output.includes('STDIN:hello'));assert.equal(snapshot.status,'running');console.log('PASS frontend disconnect/reconnect, memo persistence, live stdin');
  await pause(2100);r=await raw('poll_job',{session_id:s.session_id,job_id:upstreamJob.job_id});assert(!r.isError,JSON.stringify(r));assert.match(r.content[0].text,/UPSTREAM_RECONNECTED/);console.log('PASS upstream job survives stdio reconnection');
  await call('mux_stop',{session_id:s.session_id,job_id:j.job_id});assert.equal((await call('mux_poll',{session_id:s.session_id,job_id:j.job_id})).status,'unavailable');console.log('PASS terminal stop');
  const outputJob=await call('mux_open',{session_id:s.session_id,command:['/bin/bash','-c','for i in {1..300}; do printf "%0100d\\n" "$i"; done; exit 7']});
  const capped=await until({session_id:s.session_id,job_id:outputJob.job_id,max_output_bytes:1024},r=>r.status==='completed');assert(capped.output_bytes<=1024);assert(capped.truncated);assert.equal(capped.exit_code,7);console.log('PASS real terminal output cap and exit code');
  r=await raw('execute',{session_id:s.session_id,command:['/bin/sh','-c','head -c 100000 /dev/zero | tr "\\0" x']});assert(!r.isError);assert(Buffer.byteLength(r.content[0].text)<=65536);assert.match(r.content[0].text,/truncated/);console.log('PASS upstream output response cap');
  const longJob=JSON.parse((await raw('start_command',{session_id:s.session_id,command:['/bin/sh','-c','sleep 30']})).content[0].text);
  r=await raw('stop_job',{session_id:s.session_id,job_id:longJob.job_id});assert(!r.isError);console.log('PASS reused upstream stop_job');
  const durable=await call('mux_open',{session_id:s.session_id,command:['/bin/bash','-c','printf DAEMON_RESTART_OK; sleep 30']});
  await until({session_id:s.session_id,job_id:durable.job_id},r=>r.output.includes('DAEMON_RESTART_OK'));
  daemon.kill('SIGTERM');await once(daemon,'exit');daemon=startDaemon();
  for(let i=0;i<100;i++){try{await fs.stat(path.join(state,'backend.sock'));break;}catch{await pause(100);}}
  assert.equal((await call('mux_poll',{session_id:s.session_id,job_id:durable.job_id})).status,'running');
  assert.match((await call('get_memo',{session_id:s.session_id})).text,/reconnect/);console.log('PASS actual daemon restart preserves tmux process and memo');
  await call('close_session',{session_id:s.session_id});assert.equal((await call('list_sessions',{})).sessions.length,0);console.log('PASS explicit close');
  const leaked=spawn(process.execPath,['src/server.mjs'],{env:{...env,CONTROL_PLANE_API_KEY:'not-a-key'},stdio:'ignore'});const [exit]=await once(leaked,'exit');assert.notEqual(exit,0);console.log('PASS transport credential environment rejected');
} finally {
  await client?.close().catch(()=>{});daemon.kill('SIGTERM');
  if(daemon.exitCode===null)await once(daemon,'exit');
  try{execFileSync(env.OCI_DEV_TMUX_BIN,['-S',path.join(state,'tmux.sock'),'kill-server'],{stdio:'ignore'});}catch{}
  await fs.rm(home,{recursive:true,force:true});
  if(errors)process.stderr.write(errors);
}

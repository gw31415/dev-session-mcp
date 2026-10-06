// Runs separately from tunnel-client. Reuses real local-mcp tools unchanged.
import net from 'node:net';
import * as fs from 'node:fs/promises';
import { Client } from '@modelcontextprotocol/client';
import { StdioClientTransport } from '@modelcontextprotocol/client/stdio';
import * as z from 'zod/v4';
import { Workspace, safeEnv, MAX_OUTPUT } from './core.mjs';
import { socket, MAX_FRAME, assertCleanEnvironment } from './ipc.mjs';

process.umask(0o077);assertCleanEnvironment();
const workspace=new Workspace();await workspace.init();
const upstream=new Client({name:'oci-dev-mcp-wrapper',version:'0.1.0'});
await upstream.connect(new StdioClientTransport({command:workspace.localBin,args:['mcp'],env:safeEnv(),stderr:'ignore'}));
const upstreamTools=(await upstream.listTools()).tools;
const sid=z.string().regex(/^[A-Za-z0-9_.-]{1,64}$/).refine(v=>v!=='.'&&v!=='..');
const text=z.string().max(65536).refine(v=>!v.includes('\0'));
const output=z.number().int().min(256).max(MAX_OUTPUT).optional();
const job={session_id:sid,job_id:sid};
const extensions=new Map();
const tracked=new Map();
async function sessionInfo(s) {
  return {...await workspace.sessionInfo(s),upstream_jobs:[...(tracked.get(s)??[])].map(job_id=>({job_id}))};
}
function add(name,description,shape,action,readOnly=false) {
  const schema=z.object(shape);
  extensions.set(name,{schema,action,definition:{name,description,inputSchema:z.toJSONSchema(schema),annotations:{readOnlyHint:readOnly,destructiveHint:!readOnly,openWorldHint:true}}});
}
add('list_sessions','List local-mcp sessions, cwd, tracked upstream jobs and multiplexed terminals.',{},async()=>{
  const {sessions}=await workspace.listSessions();return {sessions:await Promise.all(sessions.map(s=>sessionInfo(s.session_id)))};
},true);
add('create_session','Create a real local-mcp start session inside tmux. Existing IDs are not overwritten.',{cwd:text.optional(),session_id:sid.optional()},a=>workspace.createSession(a));
add('connect_session','Inspect/reconnect with an existing session_id and discover job IDs after a lost response. No locking or exclusive ownership.',{session_id:sid},a=>sessionInfo(a.session_id),true);
add('get_memo','Read the explicitly saved session memo. No conversation recording.',{session_id:sid},a=>workspace.getMemo(a.session_id),true);
add('set_memo','Replace the session memo with supplied text, up to 64 KiB. Suitable for purpose/current state/next steps.',{session_id:sid,text},a=>workspace.setMemo(a));
add('mux_open','Open persistent tmux argv or an interactive bash. FULL service-user filesystem and NETWORK access, without Codex sandbox or a separate approval prompt. Use local-mcp execute for sandboxed commands.',{session_id:sid,command:z.array(text).min(1).max(256).refine(v=>v[0].length>0&&Buffer.byteLength(JSON.stringify(v))<=65536).optional(),cwd:text.optional()},a=>workspace.openMux(a));
add('mux_poll','Get bounded merged terminal output and exit status. Snapshot repeats; not an output stream.',{...job,max_output_bytes:output},a=>workspace.pollJob(a),true);
add('mux_send','Send literal stdin text and terminal keys. Enter submits a line; C-c interrupts; C-d sends EOF.',{...job,text:text.optional(),keys:z.array(z.enum(['Enter','C-c','C-d','Escape','Tab','Up','Down','Left','Right'])).max(16).optional(),max_output_bytes:output},a=>workspace.sendStdin(a));
add('mux_stop','Stop the tmux terminal. Detached/daemonized processes need explicit process management.',job,a=>workspace.stopJob(a));
add('close_session','Stop multiplexed terminals and remove session/memo metadata. Refuses when upstream jobs remain tracked.',{session_id:sid},async a=>{
  const jobs=tracked.get(a.session_id);if(jobs?.size)throw new Error('Upstream jobs remain tracked; use poll_job/stop_job first');
  return workspace.closeSession(a.session_id);
});

// Minimal tracking guards destructive close; upstream retains its own job handles.
function track(name,args,result) {
  if(['execute','start_command'].includes(name)) {
    try {const data=JSON.parse(result.content?.[0]?.text);if(data.job_id){if(!tracked.has(args.session_id))tracked.set(args.session_id,new Set());tracked.get(args.session_id).add(data.job_id);}}catch{}
  }
  if(['poll_job','stop_job'].includes(name)&&!result.isError) {
    let running=false;try{running=JSON.parse(result.content?.[0]?.text).status==='running';}catch{}
    if(!running)tracked.get(args.session_id)?.delete(args.job_id);
  }
}
function bounded(result) {
  let remaining=MAX_OUTPUT;
  return {...result,content:(result.content??[]).map(block=>{
    if(block.type!=='text')return block;
    const b=Buffer.from(block.text),marker='\n[output truncated by oci-dev-mcp]';
    const truncated=b.length>remaining;
    let end=Math.min(b.length,Math.max(0,remaining-(truncated?Buffer.byteLength(marker):0)));
    while(end>0&&end<b.length&&(b[end]&0xc0)===0x80)end--;
    const suffix=truncated&&remaining>=Buffer.byteLength(marker)?marker:'';
    remaining-=end+Buffer.byteLength(suffix);
    return {...block,text:b.subarray(0,end).toString('utf8')+suffix};
  })};
}
async function dispatch({op,args}) {
  if(op==='list')return {tools:[...upstreamTools,...[...extensions.values()].map(x=>x.definition)]};
  if(op!=='call')throw new Error('Unknown operation');
  const ext=extensions.get(args.name);
  if(ext) {
    try {return {content:[{type:'text',text:JSON.stringify(await ext.action(ext.schema.parse(args.arguments??{})))}]};}
    catch(e){return {isError:true,content:[{type:'text',text:e.code?`Operation failed (${e.code})`:String(e.message).slice(0,1024)}]};}
  }
  if(!upstreamTools.some(x=>x.name===args.name))throw new Error('Unknown tool');
  const result=await upstream.callTool(args,{timeoutMs:35000});
  track(args.name,args.arguments??{},result);
  return bounded(result);
}

// Refuse a second live backend. Only remove a stale socket after a failed connect.
try {
  await fs.lstat(socket);
  const live=await new Promise(resolve=>{const c=net.createConnection(socket);c.on('connect',()=>{c.destroy();resolve(true);});c.on('error',()=>resolve(false));});
  if(live)throw new Error('Backend already running');
  await fs.unlink(socket);
} catch(e) {if(e.code!=='ENOENT')throw e;}
const listener=net.createServer(c=>{
  let data=Buffer.alloc(0),received=false;
  c.on('error',()=>{});
  c.on('data',async b=>{
    if(received)return;
    if(data.length+b.length>MAX_FRAME){c.destroy();return;}
    data=Buffer.concat([data,b]);const end=data.indexOf(10);if(end<0)return;received=true;
    try {const result=await dispatch(JSON.parse(data.subarray(0,end).toString()));if(!c.destroyed)c.end(JSON.stringify({result})+'\n');}
    catch(e){if(!c.destroyed)c.end(JSON.stringify({error:'Tool operation failed: '+String(e.message).slice(0,1024)})+'\n');}
  });
});
await new Promise((resolve,reject)=>{listener.once('error',reject);listener.listen(socket,resolve);});
await fs.chmod(socket,0o600);
async function stop(){listener.close();await upstream.close();await fs.rm(socket,{force:true});process.exit(0);}
process.on('SIGTERM',stop);process.on('SIGINT',stop);

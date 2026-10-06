// Real Rust HTTP MCP + ephemeral local OAuth AS fixture + real tmux/sandbox.
// Fixture keys are in memory, never deployed or persisted. No production login.
import assert from 'node:assert/strict';
import * as fs from 'node:fs/promises';
import path from 'node:path';
import http from 'node:http';
import net from 'node:net';
import { generateKeyPairSync, randomBytes, createHash, sign, createHmac } from 'node:crypto';
import { spawn, execFileSync } from 'node:child_process';
import { once } from 'node:events';
import { Client, StreamableHTTPClientTransport } from '@modelcontextprotocol/client';
import { StdioClientTransport } from '@modelcontextprotocol/client/stdio';

const root=path.resolve('.'), binary=path.resolve(process.env.OCI_DEV_RUST_BIN??'rust/target/debug/oci-dev-mcp');
const home=await fs.mkdtemp('/tmp/odm-http-'),cwd=path.join(home,'project'),state=path.join(home,'state');
await fs.mkdir(cwd);
const env={PATH:process.env.PATH,HOME:home,LANG:'C.UTF-8',XDG_STATE_HOME:path.join(home,'xdg'),OCI_DEV_STATE_DIR:state,OCI_DEV_TMUX_BIN:process.env.OCI_DEV_TMUX_BIN??'tmux'};
const pause=ms=>new Promise(r=>setTimeout(r,ms));
const b64=v=>Buffer.from(v).toString('base64url');
const sha=v=>createHash('sha256').update(v).digest('base64url');
const {privateKey,publicKey}=generateKeyPairSync('rsa',{modulusLength:2048});
const jwk={...publicKey.export({format:'jwk'}),kid:'fixture-1',alg:'RS256',use:'sig'};
const codes=new Map();let issuer,resource,errors='',server,client;
function token(overrides={},key=privateKey,header={alg:'RS256',kid:jwk.kid,typ:'at+jwt'}) {
  const claims={iss:issuer,aud:resource,sub:'fixture-owner',scope:'mcp:tools',exp:Math.floor(Date.now()/1000)+600,...overrides};
  const data=`${b64(JSON.stringify(header))}.${b64(JSON.stringify(claims))}`;
  return `${data}.${sign('RSA-SHA256',Buffer.from(data),key).toString('base64url')}`;
}
function json(res,status,data){res.writeHead(status,{'Content-Type':'application/json'});res.end(JSON.stringify(data));}
const as=http.createServer(async(req,res)=>{
  const u=new URL(req.url,issuer);
  if(u.pathname==='/.well-known/oauth-authorization-server'||u.pathname==='/.well-known/openid-configuration')return json(res,200,{issuer,authorization_endpoint:issuer+'authorize',token_endpoint:issuer+'token',jwks_uri:issuer+'jwks',code_challenge_methods_supported:['S256'],response_types_supported:['code'],grant_types_supported:['authorization_code'],token_endpoint_auth_methods_supported:['none']});
  if(u.pathname==='/jwks')return json(res,200,{keys:[jwk]});
  if(u.pathname==='/authorize') {
    const q=u.searchParams;
    if(q.get('response_type')!=='code'||q.get('client_id')!=='fixture-client'||q.get('redirect_uri')!=='http://127.0.0.1/callback'||q.get('resource')!==resource||q.get('code_challenge_method')!=='S256'||!q.get('code_challenge'))return json(res,400,{error:'invalid_request'});
    const code=randomBytes(24).toString('base64url');codes.set(code,{challenge:q.get('code_challenge'),resource,redirect:q.get('redirect_uri')});
    const redirect=new URL(q.get('redirect_uri'));redirect.searchParams.set('code',code);redirect.searchParams.set('state',q.get('state'));redirect.searchParams.set('iss',issuer);
    res.writeHead(302,{Location:redirect.href});return res.end();
  }
  if(u.pathname==='/token'&&req.method==='POST') {
    let body='';for await(const data of req){body+=data; if(body.length>8192)return json(res,400,{error:'invalid_request'});}
    const q=new URLSearchParams(body),record=codes.get(q.get('code'));codes.delete(q.get('code'));
    if(!record||q.get('grant_type')!=='authorization_code'||q.get('client_id')!=='fixture-client'||q.get('redirect_uri')!==record.redirect||q.get('resource')!==record.resource||sha(q.get('code_verifier')??'')!==record.challenge)return json(res,400,{error:'invalid_grant'});
    return json(res,200,{access_token:token(),token_type:'Bearer',expires_in:600,scope:'mcp:tools'});
  }
  json(res,404,{error:'not_found'});
});
await new Promise(r=>as.listen(0,'127.0.0.1',r));issuer=`http://127.0.0.1:${as.address().port}/`;
const probe=net.createServer();await new Promise(r=>probe.listen(0,'127.0.0.1',r));const port=probe.address().port;await new Promise(r=>probe.close(r));resource=`http://127.0.0.1:${port}/mcp`;
const config=path.join(home,'http.json');
await fs.writeFile(config,JSON.stringify({listen:`127.0.0.1:${port}`,resource,issuer,allowed_subjects:['fixture-owner'],local_fixture:true}));
function start(){const p=spawn(binary,['serve','--config',config],{cwd:root,env,stdio:['ignore','ignore','pipe']});p.stderr.on('data',b=>errors+=b.toString());return p;}
async function ready(){for(let i=0;i<100;i++){if(server.exitCode!==null)throw new Error('Rust server failed: '+errors);try{if((await fetch(resource)).status===401)return;}catch{}await pause(100);}throw new Error('HTTP startup timed out: '+errors);}
async function authorize({wrongVerifier=false,wrongResource=false}={}) {
  // Hand-written fixture exchange; this does not exercise an SDK auth provider,
  // automatic client registration, or external identity-provider linking.
  const verifier=randomBytes(48).toString('base64url'),state=randomBytes(16).toString('hex');
  const url=new URL('authorize',issuer);url.search=new URLSearchParams({response_type:'code',client_id:'fixture-client',redirect_uri:'http://127.0.0.1/callback',scope:'mcp:tools',resource,state,code_challenge:sha(verifier),code_challenge_method:'S256'});
  const result=await fetch(url,{redirect:'manual'});assert.equal(result.status,302);
  const redirect=new URL(result.headers.get('location'));assert.equal(redirect.searchParams.get('state'),state);assert.equal(redirect.searchParams.get('iss'),issuer);
  const response=await fetch(new URL('token',issuer),{method:'POST',body:new URLSearchParams({grant_type:'authorization_code',client_id:'fixture-client',redirect_uri:'http://127.0.0.1/callback',code:redirect.searchParams.get('code'),code_verifier:wrongVerifier?'incorrect':verifier,resource:wrongResource?'https://wrong.invalid/mcp':resource})});
  return {status:response.status,...await response.json()};
}
async function connect(access){const c=new Client({name:'rust-http-fixture',version:'1'});await c.connect(new StreamableHTTPClientTransport(new URL(resource),{requestInit:{headers:{Authorization:`Bearer ${access}`}}}));return c;}
async function modern(access,method,params={}) {
  const headers={Authorization:`Bearer ${access}`,Accept:'application/json, text/event-stream','Content-Type':'application/json','MCP-Protocol-Version':'2026-07-28','Mcp-Method':method};
  if(params.name)headers['Mcp-Name']=params.name;
  const response=await fetch(resource,{method:'POST',headers,body:JSON.stringify({jsonrpc:'2.0',id:1,method,params:{...params,_meta:{'io.modelcontextprotocol/protocolVersion':'2026-07-28','io.modelcontextprotocol/clientInfo':{name:'modern-fixture',version:'1'},'io.modelcontextprotocol/clientCapabilities':{}}}})});
  assert.equal(response.status,200);
  assert.equal(response.headers.get('mcp-session-id'),null);
  const body=await response.text();
  const result=response.headers.get('content-type')?.includes('application/json')?JSON.parse(body):JSON.parse(body.split('\n').find(l=>l.startsWith('data:')).slice(5));
  assert(!result.error,JSON.stringify(result));return result.result;
}
async function raw(name,args){return client.callTool({name,arguments:args});}
async function call(name,args={}){const r=await raw(name,args);assert(!r.isError,JSON.stringify(r));return JSON.parse(r.content[0].text);}
async function until(args,predicate){for(let i=0;i<100;i++){const r=await call('mux_poll',args);if(predicate(r))return r;await pause(50);}throw new Error('terminal wait expired');}
try {
  server=start();await ready();
  const challenge=await fetch(resource);assert.equal(challenge.status,401);assert.match(challenge.headers.get('www-authenticate'),/resource_metadata=.*oauth-protected-resource\/mcp/);
  const metadata=await (await fetch(`http://127.0.0.1:${port}/.well-known/oauth-protected-resource/mcp`)).json();assert.equal(metadata.resource,resource);assert.deepEqual(metadata.authorization_servers,[issuer]);
  assert.equal((await authorize({wrongVerifier:true})).status,400);assert.equal((await authorize({wrongResource:true})).status,400);
  const grant=await authorize();assert.equal(grant.status,200);const access=grant.access_token;
  console.log('PASS server discovery/401, hand-written fixture code/PKCE S256/resource exchange (not SDK OAuth linking)');
  const wrongKey=generateKeyPairSync('rsa',{modulusLength:2048}).privateKey;
  for(const [label,bearer,status] of [
    ['wrong audience',token({aud:'https://wrong.invalid/mcp'}),401],['wrong issuer',token({iss:'https://wrong.invalid/'}),401],
    ['expired',token({exp:Math.floor(Date.now()/1000)-120}),401],['future nbf',token({nbf:Math.floor(Date.now()/1000)+120}),401],
    ['wrong signature',token({},wrongKey),401],['unknown kid',token({},privateKey,{alg:'RS256',kid:'missing'}),401],
    ['wrong owner',token({sub:'other-user'}),403],['missing scope',token({scope:'openid'}),403],
    ['missing exp',token({exp:undefined}),401],['missing sub',token({sub:undefined}),401],
  ]){assert.equal((await fetch(resource,{headers:{Authorization:`Bearer ${bearer}`}})).status,status,label);}
  const none=`${b64(JSON.stringify({alg:'none'}))}.${b64(JSON.stringify({sub:'fixture-owner'}))}.`;
  assert.equal((await fetch(resource,{headers:{Authorization:`Bearer ${none}`}})).status,401);
  const hsdata=`${b64(JSON.stringify({alg:'HS256',kid:jwk.kid}))}.${b64(JSON.stringify({iss:issuer,aud:resource,sub:'fixture-owner',scope:'mcp:tools',exp:Math.floor(Date.now()/1000)+60}))}`;
  assert.equal((await fetch(resource,{headers:{Authorization:`Bearer ${hsdata}.${createHmac('sha256',publicKey.export({format:'pem',type:'spki'})).update(hsdata).digest('base64url')}`}})).status,401);
  assert.equal((await fetch(resource+'?access_token='+access)).status,401);
  assert.equal((await fetch(resource,{headers:{Authorization:`Bearer ${access}`,Accept:'application/json, text/event-stream',Origin:'https://evil.invalid'}})).status,403);
  assert.equal((await fetch(resource,{headers:{Authorization:`Bearer ${access}`,Accept:'application/json, text/event-stream',Host:'evil.invalid'}})).status,400);
  console.log('PASS auth rejection: issuer/audience/signature/exp/nbf/kid/sub/scope/none/HS256/query, Origin and Host');
  assert((await modern(access,'server/discover')).supportedVersions.includes('2026-07-28'));
  assert.equal((await modern(access,'tools/list')).tools.length,20);
  const modernList=await modern(access,'tools/call',{name:'list_sessions',arguments:{}});assert.equal(modernList.resultType,'complete');
  console.log('PASS current 2026-07-28 stateless HTTP discover/list/call with protocol metadata and method/name headers');
  client=await connect(access);assert.equal((await client.listTools()).tools.length,20);console.log('PASS official SDK real Streamable HTTP initialize/tools/list: 20 tools');
  const s=await call('create_session',{session_id:'rust.test',cwd});assert.equal(s.cwd,cwd);assert.equal((await call('list_sessions')).sessions.length,1);
  await call('set_memo',{session_id:s.session_id,text:'Purpose: Rust HTTP\nNext: reconnect'});assert.match((await call('get_memo',{session_id:s.session_id})).text,/reconnect/);
  let r=await raw('execute',{session_id:s.session_id,command:['/bin/sh','-c','printf EMBEDDED_EXEC_OK']});assert(!r.isError,JSON.stringify(r));assert.match(r.content[0].text,/EMBEDDED_EXEC_OK/);
  await raw('write_file',{session_id:s.session_id,path:'edit.txt',content:'before\n'});r=await raw('write_file',{session_id:s.session_id,path:'edit.txt',content:'after\n'});assert(!r.isError,JSON.stringify(r));r=await raw('read_file',{session_id:s.session_id,path:'edit.txt'});assert.equal(r.content[0].text,'after\n');console.log('PASS embedded upstream sandbox execute + file read/write/edit, sessions + memo');
  const modernRead=await modern(access,'tools/call',{name:'read_file',arguments:{session_id:s.session_id,path:'edit.txt'}});assert.equal(modernRead.resultType,'complete');assert.equal(modernRead.content[0].text,'after\n');
  const upstreamJob=JSON.parse((await raw('start_command',{session_id:s.session_id,command:['/bin/sh','-c','sleep 2; printf UPSTREAM_HTTP_RECONNECT']})).content[0].text);
  const job=await call('mux_open',{session_id:s.session_id,command:['/bin/bash','-c','read value; printf "STDIN:%s\\n" "$value"; sleep 30']});
  await client.close();client=await connect(access);const info=await call('connect_session',{session_id:s.session_id});assert(info.mux_jobs.some(j=>j.job_id===job.job_id));assert(info.upstream_jobs.some(j=>j.job_id===upstreamJob.job_id));
  await call('mux_send',{session_id:s.session_id,job_id:job.job_id,text:'hello',keys:['Enter']});assert.equal((await until({session_id:s.session_id,job_id:job.job_id},r=>r.output.includes('STDIN:hello'))).status,'running');
  await pause(2100);r=await raw('poll_job',{session_id:s.session_id,job_id:upstreamJob.job_id});assert(!r.isError,JSON.stringify(r));assert.match(r.content[0].text,/UPSTREAM_HTTP_RECONNECT/);console.log('PASS HTTP disconnect/reconnect, job discovery, live stdin, ordinary job continuation');
  await call('mux_stop',{session_id:s.session_id,job_id:job.job_id});assert.equal((await call('mux_poll',{session_id:s.session_id,job_id:job.job_id})).status,'unavailable');
  const cappedJob=await call('mux_open',{session_id:s.session_id,command:['/bin/bash','-c','for i in {1..300}; do printf "%0100d\\n" "$i"; done; exit 7']});
  const capped=await until({session_id:s.session_id,job_id:cappedJob.job_id,max_output_bytes:1024},r=>r.status==='completed');assert(capped.output_bytes<=1024);assert(capped.truncated);assert.equal(capped.exit_code,7);
  r=await raw('execute',{session_id:s.session_id,command:['/bin/sh','-c','head -c 100000 /dev/zero | tr "\\0" x']});assert(!r.isError,JSON.stringify(r));assert(Buffer.byteLength(r.content[0].text)<=65536);assert.match(r.content[0].text,/truncated/);
  const longJob=JSON.parse((await raw('start_command',{session_id:s.session_id,command:['/bin/sh','-c','sleep 30']})).content[0].text);assert((await raw('close_session',{session_id:s.session_id})).isError);r=await raw('stop_job',{session_id:s.session_id,job_id:longJob.job_id});assert(!r.isError);console.log('PASS bounded terminal/upstream output, exit code, stop, active-job close guard');
  const durable=await call('mux_open',{session_id:s.session_id,command:['/bin/bash','-c','printf HTTP_RESTART_OK; sleep 30']});await until({session_id:s.session_id,job_id:durable.job_id},r=>r.output.includes('HTTP_RESTART_OK'));
  await client.close();server.kill('SIGTERM');await once(server,'exit');server=start();await ready();client=await connect(access);
  assert.equal((await call('mux_poll',{session_id:s.session_id,job_id:durable.job_id})).status,'running');assert.match((await call('get_memo',{session_id:s.session_id})).text,/reconnect/);console.log('PASS actual Rust server restart preserves tmux + memo');
  const clean=await call('mux_open',{session_id:s.session_id,command:['/usr/bin/env']});const environment=await until({session_id:s.session_id,job_id:clean.job_id},r=>r.status==='completed');assert(!/TOKEN|CREDENTIAL|CONTROL_PLANE|TUNNEL_|MCP_/.test(environment.output));
  await call('close_session',{session_id:s.session_id});assert.equal((await call('list_sessions')).sessions.length,0);
  const leaked=spawn(binary,['stdio'],{env:{...env,CONTROL_PLANE_API_KEY:'fixture-not-a-key'},stdio:'ignore'});assert.notEqual((await once(leaked,'exit'))[0],0);console.log('PASS clean shell environment, explicit close, credential environment rejection');
  assert(!errors.includes(access),'bearer token logged');console.log('PASS no access token in server stderr');
  await client.close();client=undefined;server.kill('SIGTERM');await once(server,'exit');
  client=new Client({name:'rust-stdio-fixture',version:'1'});await client.connect(new StdioClientTransport({command:binary,args:['stdio'],env}));assert.equal((await client.listTools()).tools.length,20);
  console.log('PASS optional Rust stdio real MCP initialize/tools/list');
} finally {
  await client?.close().catch(()=>{});server?.kill('SIGTERM');if(server?.exitCode===null)await once(server,'exit');
  try{execFileSync(env.OCI_DEV_TMUX_BIN,['-S',path.join(state,'tmux.sock'),'kill-server'],{stdio:'ignore'});}catch{}
  await new Promise(r=>as.close(r));await fs.rm(home,{recursive:true,force:true});
  if(errors)process.stderr.write(errors);
}

// Focused real-process checks for single-owner startup and AS discovery URLs.
// These are local fixtures, not client registration or external OAuth linking.
import assert from 'node:assert/strict';
import * as fs from 'node:fs/promises';
import path from 'node:path';
import http from 'node:http';
import net from 'node:net';
import { generateKeyPairSync, sign } from 'node:crypto';
import { spawn } from 'node:child_process';
import { once } from 'node:events';

const binary=path.resolve(process.env.OCI_DEV_RUST_BIN??'rust/target/debug/oci-dev-mcp');
const home=await fs.mkdtemp('/tmp/odm-auth-');
const env={PATH:process.env.PATH,HOME:home,LANG:'C.UTF-8',XDG_STATE_HOME:path.join(home,'xdg'),OCI_DEV_STATE_DIR:path.join(home,'state'),OCI_DEV_TMUX_BIN:process.env.OCI_DEV_TMUX_BIN??'tmux'};
const {privateKey,publicKey}=generateKeyPairSync('rsa',{modulusLength:2048});
const key={...publicKey.export({format:'jwk'}),kid:'focused-key',alg:'RS256',use:'sig'};
let mode,issuer,base,expected=[],requests=[],processHandle;
const as=http.createServer((req,res)=>{
  requests.push(req.url);
  const send=value=>{res.writeHead(200,{'Content-Type':'application/json'});res.end(JSON.stringify(value));};
  if(req.url==='/jwks')return send({keys:[key]});
  const selected=mode==='oauth'?expected[0]:mode==='oidc-appended'?expected[2]:expected[1];
  if(req.url===selected)return send({issuer,authorization_endpoint:base+'/authorize',token_endpoint:base+'/token',jwks_uri:base+'/jwks',code_challenge_methods_supported:['S256']});
  res.writeHead(404);res.end();
});
await new Promise(r=>as.listen(0,'127.0.0.1',r));base=`http://127.0.0.1:${as.address().port}`;
const probe=net.createServer();await new Promise(r=>probe.listen(0,'127.0.0.1',r));const port=probe.address().port;await new Promise(r=>probe.close(r));
const resource=`http://127.0.0.1:${port}/mcp`,config=path.join(home,'http.json');
const pause=ms=>new Promise(r=>setTimeout(r,ms));
const b64=s=>Buffer.from(s).toString('base64url');
function access(sub='owner'){
  const data=`${b64(JSON.stringify({alg:'RS256',kid:key.kid,typ:'at+jwt'}))}.${b64(JSON.stringify({iss:issuer,aud:resource,sub,scope:'mcp:tools',exp:Math.floor(Date.now()/1000)+60}))}`;
  return `${data}.${sign('RSA-SHA256',Buffer.from(data),privateKey).toString('base64url')}`;
}
function start(){
  let stderr='';
  const child=spawn(binary,['serve','--config',config],{env,stdio:['ignore','ignore','pipe']});
  child.stderr.on('data',b=>stderr+=b.toString());
  processHandle=child;return {child,stderr:()=>stderr};
}
async function stop(){if(processHandle?.exitCode===null){processHandle.kill('SIGTERM');await once(processHandle,'exit');}processHandle=undefined;}
async function settings(subjects){await fs.writeFile(config,JSON.stringify({listen:`127.0.0.1:${port}`,resource,issuer,allowed_subjects:subjects,local_fixture:true}));}
async function ready(child){for(let i=0;i<100;i++){assert.equal(child.exitCode,null,'unexpected startup failure');try{if((await fetch(resource)).status===401)return;}catch{}await pause(50);}throw Error('startup timed out');}
try {
  issuer=base+'/tenant/owner';mode='oauth';expected=['/.well-known/oauth-authorization-server/tenant/owner'];
  for(const subjects of [[],['owner','other'],['owner','owner'],['']]){
    requests=[];await settings(subjects);const p=start();const [code]=await once(p.child,'exit');
    assert.notEqual(code,0);assert.match(p.stderr(),/allowed_subjects must contain exactly one non-empty subject/);assert.equal(requests.length,0);
  }
  console.log('PASS startup rejects empty/multiple/duplicate/blank allowed_subjects before discovery or HTTP bind');
  for(const variant of ['oauth','oidc-inserted','oidc-appended','oidc-trailing']){
    mode=variant;const suffix='/tenant/'+variant+(variant==='oidc-trailing'?'/':'');issuer=base+suffix;
    expected=['/.well-known/oauth-authorization-server'+suffix,'/.well-known/openid-configuration'+suffix,suffix.replace(/\/$/,'')+'/.well-known/openid-configuration'];
    requests=[];await settings(['owner']);const p=start();await ready(p.child);
    const count=variant==='oauth'?1:variant==='oidc-appended'?3:2;
    assert.deepEqual(requests.slice(0,count),expected.slice(0,count));assert.equal(requests[count],'/jwks');
    const bearer=access();
    const response=await fetch(resource,{method:'POST',headers:{Authorization:`Bearer ${bearer}`,Accept:'application/json, text/event-stream','Content-Type':'application/json','MCP-Protocol-Version':'2026-07-28','Mcp-Method':'tools/list'},body:JSON.stringify({jsonrpc:'2.0',id:1,method:'tools/list',params:{_meta:{'io.modelcontextprotocol/protocolVersion':'2026-07-28','io.modelcontextprotocol/clientInfo':{name:'focused',version:'1'},'io.modelcontextprotocol/clientCapabilities':{}}}})});
    assert.equal(response.status,200);assert.equal((await response.json()).result.tools.length,20);
    assert.equal((await fetch(resource,{headers:{Authorization:`Bearer ${access('other')}`}})).status,403);
    assert.equal((await fetch(resource,{headers:{Authorization:'Bearer oauth:fixture-opaque','Cf-Access-Jwt-Assertion':bearer}})).status,401);
    assert.equal((await fetch(resource,{headers:{'Cf-Access-Jwt-Assertion':bearer}})).status,401);
    assert(!p.stderr().includes(bearer));await stop();console.log(`PASS ${variant} discovery priority, single-owner HTTP tools, different-sub/proxy-header/opaque-token rejection`);
  }
} finally {await stop();await new Promise(r=>as.close(r));await fs.rm(home,{recursive:true,force:true});}

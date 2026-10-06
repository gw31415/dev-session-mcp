import net from 'node:net';
import path from 'node:path';
import os from 'node:os';

export const state=path.resolve(process.env.OCI_DEV_STATE_DIR ?? path.join(os.homedir(),'.local/state/oci-dev-mcp'));
export const socket=path.join(state,'backend.sock');
export const MAX_FRAME=8*1024*1024;
export function assertCleanEnvironment() {
  if(Object.keys(process.env).some(k=>/^(CONTROL_PLANE_|TUNNEL_|MCP_|OPENAI_ADMIN_KEY$|CREDENTIALS_DIRECTORY$)/.test(k)))
    throw new Error('Transport environment detected: use clean-env wrapper and separate OS user');
}
export function rpc(op,args={}) {
  return new Promise((resolve,reject)=>{
    const client=net.createConnection(socket);let data=Buffer.alloc(0),done=false;
    const timer=setTimeout(()=>client.destroy(new Error('Backend request timed out')),40000);
    const finish=(e,value)=>{if(done)return;done=true;clearTimeout(timer);client.destroy();e?reject(e):resolve(value);};
    client.on('connect',()=>client.write(JSON.stringify({op,args})+'\n'));
    client.on('error',()=>finish(new Error('Backend unavailable; start the worker service')));
    client.on('end',()=>{if(!done)finish(new Error('Backend closed before response'));});
    client.on('data',b=>{
      if(data.length+b.length>MAX_FRAME)return finish(new Error('Backend response exceeds limit'));
      data=Buffer.concat([data,b]);const end=data.indexOf(10);if(end<0)return;
      try {const r=JSON.parse(data.subarray(0,end).toString());finish(r.error?new Error(r.error):null,r.result);}
      catch(e){finish(e);}
    });
  });
}

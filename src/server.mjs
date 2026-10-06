// The tunnel launches this short-lived stdio adapter. Backend jobs remain alive.
import { Server } from '@modelcontextprotocol/server';
import { StdioServerTransport } from '@modelcontextprotocol/server/stdio';
import { rpc, assertCleanEnvironment } from './ipc.mjs';

assertCleanEnvironment();
const server=new Server({name:'oci-dev-mcp',version:'0.1.0'},{capabilities:{tools:{}}});
server.setRequestHandler('tools/list',()=>rpc('list'));
server.setRequestHandler('tools/call',request=>rpc('call',request.params));
server.onclose=()=>process.exit(0);
process.on('SIGTERM',()=>process.exit(0));
process.on('SIGINT',()=>process.exit(0));
await server.connect(new StdioServerTransport());

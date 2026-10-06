import { spawn } from 'node:child_process';
import * as fs from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import { randomUUID } from 'node:crypto';
import { fileURLToPath } from 'node:url';

const worker = fileURLToPath(new URL('./worker.mjs', import.meta.url));
const idPattern = /^[A-Za-z0-9_.-]{1,64}$/;
const forbiddenEnv = /^(CONTROL_PLANE_|TUNNEL_|OPENAI_ADMIN_KEY$|MCP_|CREDENTIALS_DIRECTORY$)/;
export const MAX_OUTPUT = 65536;

function id(value) {
  if (!idPattern.test(value) || value === '.' || value === '..') throw new Error('Invalid session/job ID');
  return value;
}
function tail(text, limit) {
  const data = Buffer.from(text);
  let start = Math.max(0, data.length - limit);
  while (start < data.length && (data[start] & 0xc0) === 0x80) start++;
  return { output: data.subarray(start).toString('utf8'), output_bytes: data.length - start, truncated: start > 0 };
}
export function safeEnv() {
  const env = { PATH: process.env.PATH ?? '/usr/local/bin:/usr/bin:/bin', HOME: os.homedir(), LANG: 'C.UTF-8', TERM: 'xterm-256color' };
  for (const k of ['USER', 'LOGNAME', 'SHELL', 'TMPDIR', 'XDG_STATE_HOME']) if (process.env[k]) env[k] = process.env[k];
  return env;
}
function run(program, args, { input, maxBytes = 4 * 1024 * 1024 } = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(program, args, { env: safeEnv(), stdio: ['pipe', 'pipe', 'pipe'] });
    const stdout = [], stderr = []; let bytes = 0, failed = false;
    const timer = setTimeout(() => { failed = true; child.kill('SIGKILL'); reject(new Error('tmux operation timed out')); }, 10000);
    child.on('error', () => { clearTimeout(timer); reject(new Error('tmux executable unavailable')); });
    child.stdout.on('data', data => {
      bytes += data.length;
      if (bytes > maxBytes) { failed = true; child.kill('SIGKILL'); reject(new Error('tmux response exceeded internal limit')); }
      else stdout.push(data);
    });
    child.stderr.on('data', data => { if (stderr.reduce((n, b) => n + b.length, 0) < 4096) stderr.push(data); });
    child.stdin.on('error', () => {});
    child.stdin.end(input);
    child.on('close', code => {
      clearTimeout(timer);
      if (!failed) resolve({ code, stdout: Buffer.concat(stdout).toString('utf8'), stderr: Buffer.concat(stderr).toString('utf8') });
    });
  });
}

export class Workspace {
  constructor() {
    this.state = path.resolve(process.env.OCI_DEV_STATE_DIR ?? path.join(os.homedir(), '.local/state/oci-dev-mcp'));
    this.defaultCwd = path.resolve(process.env.OCI_DEV_DEFAULT_CWD ?? os.homedir());
    this.tmuxBin = process.env.OCI_DEV_TMUX_BIN ?? 'tmux';
    this.socket = path.join(this.state, 'tmux.sock');
    this.config = path.join(this.state, 'tmux.conf');
    this.localState = path.join(process.env.XDG_STATE_HOME ?? path.join(os.homedir(), '.local/state'), 'local-mcp');
    this.localBin = process.env.OCI_DEV_LOCAL_MCP_BIN ?? path.resolve('vendor/local-mcp/target/debug/local-mcp');
    if (Buffer.byteLength(this.socket) > 100) throw new Error('State directory is too long for a Unix socket');
  }
  async init() {
    // Fail closed when a transport launcher accidentally passes its secrets.
    if (Object.keys(process.env).some(k => forbiddenEnv.test(k))) throw new Error('Transport environment detected; launch MCP with a clean environment and separate OS user');
    await fs.mkdir(path.join(this.state, 'sessions'), { recursive: true, mode: 0o700 });
    await fs.chmod(this.state, 0o700);
    await fs.writeFile(this.config, 'set -g history-limit 10000\nset -g remain-on-exit on\nset -g status off\nset -g default-shell /bin/bash\n', { mode: 0o600 });
    const r = await run(this.tmuxBin, ['-V']);
    if (r.code !== 0) throw new Error('tmux is required');
  }
  sessionDir(s) { return path.join(this.state, 'sessions', id(s)); }
  jobDir(s, j) { return path.join(this.sessionDir(s), 'jobs', id(j)); }
  tmuxName(j) { return `odm_${id(j)}`; }
  async tmux(args, options, allowFailure = false) {
    const r = await run(this.tmuxBin, ['-S', this.socket, '-f', this.config, ...args], options);
    if (r.code !== 0 && !allowFailure) throw new Error(`tmux operation failed: ${r.stderr.trim().slice(0,512)}`);
    return r;
  }
  async session(s) {
    const record = JSON.parse(await fs.readFile(path.join(this.localState,'sessions', `${id(s)}.json`), 'utf8'));
    return {...record,session_id:record.id};
  }
  async job(s, j) {
    await this.session(s);
    return JSON.parse(await fs.readFile(path.join(this.jobDir(s,j), 'job.json'), 'utf8'));
  }
  async directory(s, value) {
    const session = await this.session(s);
    const resolved = await fs.realpath(path.resolve(session.cwd, value ?? '.'));
    if (!(await fs.stat(resolved)).isDirectory()) throw new Error('cwd must be a directory');
    return resolved;
  }
  async createSession({ cwd, session_id }) {
    const s = id(session_id ?? randomUUID());
    const resolved = await fs.realpath(path.resolve(cwd ?? this.defaultCwd));
    if (!(await fs.stat(resolved)).isDirectory()) throw new Error('cwd must be a directory');
    try {await this.session(s); throw new Error('Session already exists; use connect_session');}
    catch(e) {if(e.code!=='ENOENT') throw e;}
    await fs.mkdir(path.join(this.sessionDir(s),'jobs'),{recursive:true,mode:0o700});
    await this.startCommandRaw(s,'approvals',[this.localBin,'start',s],resolved);
    for(let i=0;i<50;i++) {
      try {return await this.sessionInfo(s);} catch(e) {if(e.code!=='ENOENT') throw e;}
      await new Promise(r=>setTimeout(r,100));
    }
    throw new Error('local-mcp start did not create session metadata');
  }
  async listSessions() {
    let files;
    try {files=await fs.readdir(path.join(this.localState,'sessions'));} catch(e) {if(e.code==='ENOENT') return {sessions:[]};throw e;}
    const sessions=[];
    for(const f of files) if(f.endsWith('.json') && idPattern.test(f.slice(0,-5))) sessions.push(await this.sessionInfo(f.slice(0,-5)));
    return {sessions};
  }
  async sessionInfo(s) {
    const record=await this.session(s),jobs=[];
    let files=[];
    try {files=await fs.readdir(path.join(this.sessionDir(s),'jobs'));} catch(e) {if(e.code!=='ENOENT') throw e;}
    for(const j of files) {
      const job=await this.job(s,j);
      jobs.push({job_id:j,kind:job.kind,created_at:job.created_at,...await this.status(j)});
    }
    return {...record,mux_jobs:jobs,memo_available:await fs.stat(path.join(this.sessionDir(s),'memo.md')).then(()=>true,()=>false)};
  }
  async getMemo(s) {
    await this.session(s);
    let text='';
    try {text=await fs.readFile(path.join(this.sessionDir(s),'memo.md'),'utf8');} catch(e) {if(e.code!=='ENOENT') throw e;}
    return {session_id:s,text};
  }
  async setMemo({session_id:s,text}) {
    await this.session(s);
    if(Buffer.byteLength(text)>65536) throw new Error('Memo exceeds 64 KiB');
    await fs.mkdir(this.sessionDir(s),{recursive:true,mode:0o700});
    const tmp=path.join(this.sessionDir(s),`memo-${randomUUID()}.tmp`);
    await fs.writeFile(tmp,text,{mode:0o600,flag:'wx'});
    await fs.rename(tmp,path.join(this.sessionDir(s),'memo.md'));
    return {session_id:s,saved:true,bytes:Buffer.byteLength(text)};
  }
  async status(j) {
    const exists=await this.tmux(['has-session','-t',`=${this.tmuxName(j)}`],undefined,true);
    if(exists.code!==0)return {status:'unavailable',exit_code:null};
    const r = await this.tmux(['display-message', '-p', '-t', `${this.tmuxName(j)}:0.0`, '#{pane_dead}|#{pane_dead_status}|#{pane_pid}|#{history_size}'], undefined, true);
    if (r.code !== 0) return { status: 'unavailable', exit_code: null };
    const [dead, code, pid, history] = r.stdout.trim().split('|');
    return { status: dead === '1' ? 'completed' : 'running', exit_code: dead === '1' && code !== '' ? Number(code) : null, pid: Number(pid), history_lines: Number(history) };
  }
  async startCommandRaw(s,kind,command,cwd) {
    const j=randomUUID(),dir=this.jobDir(s,j);
    await fs.mkdir(dir,{recursive:true,mode:0o700});
    const record={session_id:s,job_id:j,kind,created_at:new Date().toISOString(),cwd};
    await fs.writeFile(path.join(dir,'job.json'),JSON.stringify(record),{mode:0o600});
    await fs.writeFile(path.join(dir,'spec.json'),JSON.stringify({command,cwd,env:safeEnv()}),{mode:0o600});
    await this.tmux(['new-session','-d','-s',this.tmuxName(j),'-x','160','-y','40','-c',cwd,process.execPath,worker,path.join(dir,'spec.json')]);
    return {session_id:s,job_id:j};
  }
  async openMux({session_id:s,command,cwd}) {
    await this.session(s);
    const job=await this.startCommandRaw(s,'terminal',command ?? ['/bin/bash','--noprofile','--norc','-i'],await this.directory(s,cwd));
    return this.pollJob(job);
  }
  async pollJob({ session_id:s, job_id:j, max_output_bytes = 16384 }) {
    await this.job(s,j);
    const status = await this.status(j);
    if (status.status === 'unavailable') return {session_id:s,job_id:j,...status,output:'',output_bytes:0,truncated:false};
    const r = await this.tmux(['capture-pane','-p','-J','-t',`${this.tmuxName(j)}:0.0`,'-S','-10000'],undefined,true);
    if(r.code!==0)return {session_id:s,job_id:j,status:'unavailable',exit_code:null,output:'',output_bytes:0,truncated:false};
    // A bounded terminal snapshot, not an append-only output log.
    const output = r.stdout.replace(/\n+$/, '\n');
    return { session_id:s,job_id:j,...status,...tail(output,Math.min(max_output_bytes,MAX_OUTPUT)),snapshot:true,history_may_be_truncated:status.history_lines>=10000 };
  }
  async sendStdin({session_id:s,job_id:j,text='',keys=[],max_output_bytes}) {
    await this.job(s,j);
    if ((await this.status(j)).status !== 'running') throw new Error('Job is not running');
    if (text) await this.tmux(['send-keys','-l','-t',`${this.tmuxName(j)}:0.0`,'--',text]);
    if (keys.length) await this.tmux(['send-keys','-t',`${this.tmuxName(j)}:0.0`,...keys]);
    return this.pollJob({session_id:s,job_id:j,max_output_bytes});
  }
  async stopJob({session_id:s,job_id:j}) {
    await this.job(s,j);
    const before = await this.pollJob({session_id:s,job_id:j});
    await this.tmux(['kill-session','-t',`=${this.tmuxName(j)}`],undefined,true);
    return {...before,status:'stopped',exit_code:null};
  }
  async closeSession(s) {
    const record=await this.sessionInfo(s);
    for(const j of record.mux_jobs) await this.stopJob({session_id:s,job_id:j.job_id});
    await fs.rm(path.join(this.localState,'sessions',`${id(s)}.json`),{force:true});
    await fs.rm(this.sessionDir(s),{recursive:true,force:true});
    return {session_id:s,status:'closed'};
  }
}

// tmux owns this process, not the MCP connection. No command/output logging.
import { readFile } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { constants } from 'node:os';

const spec = JSON.parse(await readFile(process.argv[2], 'utf8'));
const child = spawn(spec.command[0], spec.command.slice(1), {
  cwd: spec.cwd, env: spec.env, stdio: 'inherit'
});
child.once('error', () => { process.stderr.write('Could not start command\n'); process.exit(127); });
child.once('exit', (code, signal) => process.exit(code ?? 128 + (constants.signals[signal] ?? 0)));

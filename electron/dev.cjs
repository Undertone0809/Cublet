const { spawn, spawnSync } = require('node:child_process');
const net = require('node:net');
const path = require('node:path');

const root = path.join(__dirname, '..');
const port = Number(process.env.VITE_PORT || 1420);
const url = `http://127.0.0.1:${port}`;

function waitForPort(timeoutMs = 30_000) {
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + timeoutMs;
    const probe = () => {
      const socket = net.createConnection({ host: '127.0.0.1', port });
      socket.once('connect', () => { socket.destroy(); resolve(); });
      socket.once('error', () => {
        socket.destroy();
        if (Date.now() >= deadline) reject(new Error(`Vite did not start on ${url}`));
        else setTimeout(probe, 100);
      });
    };
    probe();
  });
}

const build = spawnSync('cargo', ['build', '--manifest-path', 'rust-core/Cargo.toml', '--bin', 'codex-core'], {
  cwd: root,
  stdio: 'inherit',
});
if (build.status !== 0) process.exit(build.status || 1);

const childEnv = { ...process.env };
delete childEnv.ELECTRON_RUN_AS_NODE;

const vite = spawn('npm', ['run', 'dev', '--', '--host', '127.0.0.1', '--port', String(port)], {
  cwd: root,
  stdio: 'inherit',
  env: childEnv,
});
const electron = () => spawn('electron', ['.', ...process.argv.slice(2)], {
  cwd: root,
  stdio: 'inherit',
  env: { ...childEnv, ELECTRON_START_URL: url },
});

let desktop;
let shuttingDown = false;
function stop() {
  if (shuttingDown) return;
  shuttingDown = true;
  if (desktop && !desktop.killed) desktop.kill('SIGTERM');
  if (vite && !vite.killed) vite.kill('SIGTERM');
}
process.once('SIGINT', () => { stop(); process.exit(130); });
process.once('SIGTERM', () => { stop(); process.exit(143); });
vite.once('exit', (code) => {
  if (!shuttingDown && code !== 0) { stop(); process.exit(code || 1); }
});

waitForPort().then(() => {
  desktop = electron();
  desktop.once('exit', (code) => { stop(); process.exit(code || 0); });
}).catch((error) => {
  console.error(error);
  stop();
  process.exit(1);
});

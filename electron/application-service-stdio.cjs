'use strict';

// Only the bundled MCP binary launches this companion. No window, shell, or
// network listener is needed; plans are bound to this local stdio session.
const readline = require('node:readline');
const path = require('node:path');
const os = require('node:os');
const { spawn } = require('node:child_process');
const { createApplicationService } = require('./application-service.cjs');
const RESPONSE_LIMIT = 1024 * 1024;

function encodeResponse(response) {
  const line = JSON.stringify(response) + '\n';
  return Buffer.byteLength(line, 'utf8') <= RESPONSE_LIMIT ? line
    : JSON.stringify({ ok: false, error: 'Application result exceeded the response limit. Search with a smaller page or preview fewer applications. If execution was requested, check the application state before retrying.' }) + '\n';
}

const core = process.argv[2];
if (!core || !path.isAbsolute(core)) throw new Error('Missing Cublet backend path');
const app = { getPath: key => key === 'home' ? os.homedir() : process.execPath };

function invokeNative(method, params) {
  return new Promise((resolve, reject) => {
    const env = { ...process.env };
    delete env.ELECTRON_RUN_AS_NODE;
    const child = spawn(core, [], { env, stdio: ['pipe', 'pipe', 'ignore'] });
    let output = '';
    child.on('error', reject);
    child.stdin.on('error', reject);
    child.stdout.setEncoding('utf8');
    child.stdout.on('data', chunk => {
      output += chunk;
      if (Buffer.byteLength(output, 'utf8') > RESPONSE_LIMIT) {
        child.kill();
        reject(new Error('Invalid Cublet backend response'));
      }
    });
    child.on('close', code => {
      try {
        const result = JSON.parse(output);
        if (code !== 0 || !result.ok) throw new Error(result.error || 'Cublet backend exited');
        resolve(result.result);
      } catch (error) { reject(error); }
    });
    child.stdin.end(JSON.stringify({ id: 1, method, params }) + '\n');
  });
}

const service = createApplicationService({ app, invokeNative });
const input = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });

async function run() {
  for await (const line of input) {
    try {
      const { method, params } = JSON.parse(line);
      let result;
      if (method === 'search_applications') {
        const { applications, ...summary } = await service.search(params);
        const offset = params.offset ?? 0;
        const end = offset + (params.limit ?? 50);
        result = { ...summary, total: applications.length, applications: applications.slice(offset, end) };
      } else if (method === 'preview_app_uninstall') {
        // Refresh discovery to authorize explicitly requested standalone apps.
        const matches = await service.search({ query: '', refresh: true });
        service.rememberApplications(matches.applications);
        result = await service.preview(params.paths, params.retryPlanId);
      } else if (method === 'execute_app_uninstall' && params.confirmed === true) {
        result = await service.execute(params.planId, params.selectedIds, params.mode);
      } else {
        throw new Error('Unsupported application operation');
      }
      process.stdout.write(encodeResponse({ ok: true, result }));
    } catch (error) {
      process.stdout.write(encodeResponse({ ok: false, error: error.message }));
    }
  }
}

void run().catch(error => { process.stderr.write(error.message + '\n'); process.exitCode = 1; });

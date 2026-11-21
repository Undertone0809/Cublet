const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { connectionDetails } = require('./agent-connection.cjs');

test('connection prompt contains exact stdio executable and arguments', () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'cublet connection-'));
  try {
    const executable = path.join(directory, 'cleaner "quoted"');
    fs.writeFileSync(executable, '', { mode: 0o700 });
    for (const locale of ['zh', 'en']) {
      const connection = connectionDetails(executable, locale);
      assert.equal(connection.name, 'cublet-cleaner');
      assert.equal(connection.transport, 'stdio');
      assert.equal(connection.command, executable);
      assert.deepEqual(connection.args, ['--mcp', '--profile', 'cleanup', '--allow-cleanup']);
      const lines = connection.prompt.split('\n');
      assert.ok(lines.includes('name: cublet-cleaner'));
      assert.ok(lines.includes('transport: stdio'));
      assert.equal(JSON.parse(lines.find(line => line.startsWith('command: ')).slice(9)), executable);
      assert.deepEqual(JSON.parse(lines.find(line => line.startsWith('args: ')).slice(6)), connection.args);
      assert.match(connection.prompt, /reclaim_space and cleanup_status|reclaim_space 和 cleanup_status/);
      assert.match(connection.prompt, /Reuse the existing entry|已有配置就沿用/);
    }
    assert.throws(() => connectionDetails('relative/core'), /absolute/);
    assert.throws(() => connectionDetails(path.join(directory, 'missing')), /ENOENT/);
  } finally { fs.rmSync(directory, { recursive: true, force: true }); }
});

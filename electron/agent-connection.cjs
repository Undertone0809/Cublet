const fs = require('node:fs');
const path = require('node:path');

function connectionDetails(command, locale = 'zh') {
  if (!path.isAbsolute(command)) throw new Error('Cublet Cleaner command must be absolute');
  fs.accessSync(command, fs.constants.X_OK);
  const name = 'cublet-cleaner';
  const transport = 'stdio';
  const args = ['--mcp', '--profile', 'cleanup', '--allow-cleanup'];
  const settings = [
    `name: ${name}`,
    `transport: ${transport}`,
    `command: ${JSON.stringify(command)}`,
    `args: ${JSON.stringify(args)}`,
  ].join('\n');
  const prompt = locale === 'en'
    ? `Add Cublet Cleaner to your client's MCP settings. Reuse the existing entry if it's already configured.\n\n${settings}\n\nCheck that reclaim_space and cleanup_status are available. Run cleanup only when I authorize it.`
    : `请在当前客户端的 MCP 设置中接入 Cublet Cleaner。已有配置就沿用。\n\n${settings}\n\n确认 reclaim_space 和 cleanup_status 可用。只有我授权时才执行清理。`;
  return { name, transport, command, args, prompt };
}

module.exports = { connectionDetails };

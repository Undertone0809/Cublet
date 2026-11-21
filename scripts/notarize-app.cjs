const { execFileSync } = require('node:child_process');
const { mkdtempSync, rmSync, writeFileSync } = require('node:fs');
const { tmpdir } = require('node:os');
const path = require('node:path');

module.exports = async function afterSign(context) {
  if (process.env.CUBLET_NOTARIZE !== '1') return;
  if (context.electronPlatformName !== 'darwin') throw new Error('macOS notarization hook used on another platform');
  const app = path.join(context.appOutDir, `${context.packager.appInfo.productFilename}.app`);
  const profile = process.env.CUBLET_NOTARY_PROFILE;
  if (!profile) throw new Error('Set CUBLET_NOTARY_PROFILE to the notarization Keychain profile');
  const temp = mkdtempSync(path.join(tmpdir(), 'cublet-notary-'));
  const run = (command, args, capture = false) => execFileSync(command, args, { stdio: capture ? ['ignore', 'pipe', 'inherit'] : 'inherit', encoding: 'utf8' });
  try {
    run('codesign', ['--verify', '--deep', '--strict', app]);
    const zip = path.join(temp, 'submission.zip');
    run('ditto', ['-c', '-k', '--sequesterRsrc', '--keepParent', app, zip]);
    const result = JSON.parse(run('xcrun', ['notarytool', 'submit', zip, '--keychain-profile', profile, '--wait', '--output-format', 'json'], true));
    writeFileSync(path.join(context.appOutDir, 'app-notarization.json'), JSON.stringify({ id: result.id, status: result.status }, null, 2));
    if (result.status !== 'Accepted') throw new Error(`App notarization ${result.status}; submission ${result.id}`);
    run('xcrun', ['stapler', 'staple', app]);
    run('xcrun', ['stapler', 'validate', app]);
  } finally { rmSync(temp, { recursive: true, force: true }); }
};

const path = require('node:path');
function configureAppIdentity(app) {
  const appData = app.getPath('appData');
  const existing = app.getPath('userData');
  const profile = app.commandLine?.getSwitchValue('user-data-dir');
  // Native fixture bootstraps may supply a disposable profile in development.
  // Production always preserves the real pre-Cublet profile, before sessions/windows.
  if (profile && path.isAbsolute(profile)) {
    app.setPath('userData', profile);
  } else if (app.isPackaged || existing === path.join(appData, app.getName())) {
    app.setPath('userData', path.join(appData, 'mac-storage-manager'));
  }
  app.setName('Cublet');
}
module.exports = { configureAppIdentity };

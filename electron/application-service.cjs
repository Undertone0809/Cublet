'use strict';

const { createApplicationDiscovery } = require('./application-discovery.cjs');
const { createAppUninstaller, measureBytes } = require('./app-uninstall.cjs');

function createApplicationService({ app, invokeNative, ...options }) {
  const discovery = createApplicationDiscovery({ home: app.getPath('home'), ...options });
  const uninstaller = createAppUninstaller({
    app, ...options,
    getDisposition: () => invokeNative('get_cleanup_disposition', {}),
    sealApplicationItem: (stamps) => invokeNative('seal_application_item', { stamps }),
    dispose: (_item, mode, { stamps, seal }) =>
      invokeNative('dispose_application_item', { mode, stamps, seal }),
  });

  async function search(params = {}) {
    const result = await discovery.search(params);
    uninstaller.rememberApplications(result.applications);
    // A focused result can show useful sizes immediately. Keep an unfiltered
    // inventory fast and use the existing disk snapshot there when available.
    if (params.query?.trim() && result.applications.length <= 12) {
      const rows = result.applications.values();
      await Promise.all(Array.from({ length: 4 }, async () => {
        for (const row of rows) {
          try {
            row.bundleBytes = await measureBytes(row.path);
            row.bundleBytesKnown = true;
          } catch { /* Unmeasured remains explicit. */ }
        }
      }));
    }
    return result;
  }

  return { ...uninstaller, search };
}

module.exports = { createApplicationService };

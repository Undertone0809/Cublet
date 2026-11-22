# Cublet changelog

## 0.6.2 · 2026-11-22

- Retry transient macOS directory read interruptions so a brief interruption does not prevent a scan from being saved.
- Keep the last complete scan during refresh, including when cache space is tight.
- Growth shows the latest saved scan time and offers a retry when the scan is incomplete or could not be saved.

Available for Apple Silicon and Intel Macs running macOS 13 or later.

## 0.6.1 · 2026-11-21

- Uninstall previews show a single notice with the paths of confirmed app copies. Precisely matched related files stay selectable when an unrelated discovery source has incomplete coverage.
- Appearance follows macOS by default. Light and Dark overrides are still available and existing explicit preferences are kept.
- The bundled Cublet Cleaner reports the app version. Existing connections started with this release can switch to the new worker after future app updates, once accepted work has safely finished.

## 0.6.0 · 2026-11-21

- Search Applications directly by name or path. The results distinguish installed copies, renamed backups and discovered builds.
- Review related caches, preferences and app data before uninstalling. Shared or unverified items show why they are protected.
- Application uninstall defaults to permanent deletion. Select Move to Trash in Settings to retain items in Finder Trash.
- Review failed items again after a partial uninstall. Advanced Cublet Cleaner connections can search, preview and run explicitly authorized application cleanup with the same preference.

const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('electronAPI', Object.freeze({
  updates: Object.freeze({
    command(command) {
      if (!['state', 'check', 'download', 'install'].includes(command)) return Promise.reject(new Error('Invalid update command'));
      return ipcRenderer.invoke('updates:command', command);
    },
    subscribe(listener) {
      const handler = (_event, state) => listener(state);
      ipcRenderer.on('updates:state', handler);
      return () => ipcRenderer.removeListener('updates:state', handler);
    },
  }),
  invoke(method, params = {}) {
    return ipcRenderer.invoke('core:invoke', { method, params });
  },
}));

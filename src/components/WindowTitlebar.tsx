import { useLayoutEffect } from 'react';

function isNativeMacWindow() {
  return typeof navigator !== 'undefined'
    && navigator.userAgent.includes('Macintosh')
    && navigator.userAgent.includes('Electron/');
}

export function WindowTitlebar({ label }: { label: string }) {
  const native = isNativeMacWindow();

  useLayoutEffect(() => {
    if (!native) return;
    document.documentElement.classList.add('mac-native-window');
    return () => document.documentElement.classList.remove('mac-native-window');
  }, [native]);

  return (
    <header className="window-titlebar">
      <h1 className="window-titlebar-title">{label}</h1>
    </header>
  );
}

import { createContext, useContext, useEffect, useLayoutEffect, useMemo, useState, type ReactNode } from 'react';
import { applyTheme, prefersDarkTheme, readStoredTheme, resolveTheme, THEME_STORAGE_KEY, type ResolvedTheme, type Theme } from './theme';

type ThemeContextValue = {
  theme: Theme;
  resolvedTheme: ResolvedTheme;
  setTheme: (theme: Theme) => void;
};

const ThemeContext = createContext<ThemeContextValue | null>(null);

function syncNativeTheme(theme: Theme) {
  // Send the preference, never its resolved color: an explicit color would pin
  // Electron's media query and native chrome after selecting System.
  void window.electronAPI?.invoke('set_window_appearance', { theme }).catch(() => {});
}

export function ThemeProvider({ children }: { children: ReactNode }) {
  const [theme, setTheme] = useState<Theme>(readStoredTheme);
  const [prefersDark, setPrefersDark] = useState(prefersDarkTheme);
  const resolvedTheme = resolveTheme(theme, prefersDark);

  useEffect(() => {
    const media = window.matchMedia?.('(prefers-color-scheme: dark)');
    if (!media) return;
    const update = () => setPrefersDark(media.matches);
    media.addEventListener('change', update);
    update();
    return () => media.removeEventListener('change', update);
  }, []);

  useLayoutEffect(() => {
    applyTheme(resolvedTheme);
  }, [resolvedTheme]);

  useLayoutEffect(() => {
    syncNativeTheme(theme);
  }, [theme]);

  useEffect(() => {
    try {
      window.localStorage.setItem(THEME_STORAGE_KEY, theme);
    } catch {
      // Local persistence is best-effort in restricted webviews.
    }
  }, [theme]);

  const value = useMemo(() => ({ theme, resolvedTheme, setTheme }), [theme, resolvedTheme]);
  return <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>;
}

export function useTheme() {
  const context = useContext(ThemeContext);
  if (!context) throw new Error('useTheme must be used inside ThemeProvider');
  return context;
}

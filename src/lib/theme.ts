export type Theme = 'system' | 'light' | 'dark';
export type ResolvedTheme = 'light' | 'dark';

export const THEME_STORAGE_KEY = 'codex-storage-manager.theme.v1';

export function parseStoredTheme(value: string | null): Theme {
  return value === 'light' || value === 'dark' ? value : 'system';
}

export function resolveTheme(theme: Theme | null, prefersDark: boolean): ResolvedTheme {
  return theme === 'light' || theme === 'dark' ? theme : (prefersDark ? 'dark' : 'light');
}

export function readStoredTheme(): Theme {
  if (typeof window === 'undefined') return 'system';
  try {
    return parseStoredTheme(window.localStorage.getItem(THEME_STORAGE_KEY));
  } catch {
    // Local persistence is best-effort in restricted webviews.
    return 'system';
  }
}

export function prefersDarkTheme(): boolean {
  return typeof window !== 'undefined' && (window.matchMedia?.('(prefers-color-scheme: dark)').matches ?? false);
}

export function applyTheme(theme: ResolvedTheme) {
  const root = document.documentElement;
  root.classList.toggle('dark', theme === 'dark');
  root.dataset.theme = theme;
  root.style.colorScheme = theme;
}

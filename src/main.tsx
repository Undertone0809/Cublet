import React from 'react';
import ReactDOM from 'react-dom/client';
import '@fontsource-variable/outfit';
import App from './MacApp';
import { I18nProvider } from './lib/i18n';
import { applyTheme, prefersDarkTheme, readStoredTheme, resolveTheme } from './lib/theme';
import { ThemeProvider } from './lib/theme-context';
import './styles.css';

applyTheme(resolveTheme(readStoredTheme(), prefersDarkTheme()));

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <ThemeProvider>
      <I18nProvider><App /></I18nProvider>
    </ThemeProvider>
  </React.StrictMode>,
);

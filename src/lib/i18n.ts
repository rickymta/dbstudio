import i18n from 'i18next';
import { initReactI18next } from 'react-i18next';
import enCommon from '../locales/en/common.json';
import enConnections from '../locales/en/connections.json';
import enData from '../locales/en/data.json';
import enExplorer from '../locales/en/explorer.json';
import viCommon from '../locales/vi/common.json';
import viConnections from '../locales/vi/connections.json';
import viData from '../locales/vi/data.json';
import viExplorer from '../locales/vi/explorer.json';

export const LANGUAGES = ['vi', 'en'] as const;
export type Language = (typeof LANGUAGES)[number];

/** Translation namespaces, one JSON file per language each. `common` is the default. */
export const NAMESPACES = ['common', 'connections', 'explorer', 'data'] as const;
export type Namespace = (typeof NAMESPACES)[number];

const STORAGE_KEY = 'dbstudio.language';

function isLanguage(value: unknown): value is Language {
  return typeof value === 'string' && (LANGUAGES as readonly string[]).includes(value);
}

/** Stored choice, else system language (vi or en), else en. */
export function detectLanguage(): Language {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (isLanguage(stored)) return stored;
  } catch {
    // storage unavailable
  }
  const system = typeof navigator !== 'undefined' ? navigator.language.toLowerCase() : 'en';
  return system.startsWith('vi') ? 'vi' : 'en';
}

export function setLanguage(language: Language): void {
  try {
    localStorage.setItem(STORAGE_KEY, language);
  } catch {
    // storage unavailable
  }
  void i18n.changeLanguage(language);
}

void i18n.use(initReactI18next).init({
  resources: {
    en: { common: enCommon, connections: enConnections, explorer: enExplorer, data: enData },
    vi: { common: viCommon, connections: viConnections, explorer: viExplorer, data: viData },
  },
  ns: [...NAMESPACES],
  defaultNS: 'common',
  lng: detectLanguage(),
  fallbackLng: 'en',
  interpolation: { escapeValue: false },
});

i18n.on('languageChanged', (lng) => {
  document.documentElement.lang = lng;
});
document.documentElement.lang = i18n.language;

export default i18n;

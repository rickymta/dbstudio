import i18n from 'i18next';
import { initReactI18next } from 'react-i18next';
import enBackup from '../locales/en/backup.json';
import enCommon from '../locales/en/common.json';
import enConnections from '../locales/en/connections.json';
import enData from '../locales/en/data.json';
import enDocument from '../locales/en/document.json';
import enEditor from '../locales/en/editor.json';
import enErd from '../locales/en/erd.json';
import enExplorer from '../locales/en/explorer.json';
import enExport from '../locales/en/export.json';
import enJobs from '../locales/en/jobs.json';
import enKeyvalue from '../locales/en/keyvalue.json';
import enPlan from '../locales/en/plan.json';
import enSafety from '../locales/en/safety.json';
import enSettings from '../locales/en/settings.json';
import viBackup from '../locales/vi/backup.json';
import viCommon from '../locales/vi/common.json';
import viConnections from '../locales/vi/connections.json';
import viData from '../locales/vi/data.json';
import viDocument from '../locales/vi/document.json';
import viEditor from '../locales/vi/editor.json';
import viErd from '../locales/vi/erd.json';
import viExplorer from '../locales/vi/explorer.json';
import viExport from '../locales/vi/export.json';
import viJobs from '../locales/vi/jobs.json';
import viKeyvalue from '../locales/vi/keyvalue.json';
import viPlan from '../locales/vi/plan.json';
import viSafety from '../locales/vi/safety.json';
import viSettings from '../locales/vi/settings.json';

export const LANGUAGES = ['vi', 'en'] as const;
export type Language = (typeof LANGUAGES)[number];

/** Translation namespaces, one JSON file per language each. `common` is the default. */
export const NAMESPACES = [
  'common',
  'connections',
  'explorer',
  'data',
  'settings',
  'editor',
  'plan',
  'erd',
  'export',
  'jobs',
  'backup',
  'document',
  'keyvalue',
  'safety',
] as const;
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
    en: {
      common: enCommon,
      connections: enConnections,
      explorer: enExplorer,
      data: enData,
      settings: enSettings,
      editor: enEditor,
      plan: enPlan,
      erd: enErd,
      export: enExport,
      jobs: enJobs,
      backup: enBackup,
      document: enDocument,
      keyvalue: enKeyvalue,
      safety: enSafety,
    },
    vi: {
      common: viCommon,
      connections: viConnections,
      explorer: viExplorer,
      data: viData,
      settings: viSettings,
      editor: viEditor,
      plan: viPlan,
      erd: viErd,
      export: viExport,
      jobs: viJobs,
      backup: viBackup,
      document: viDocument,
      keyvalue: viKeyvalue,
      safety: viSafety,
    },
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

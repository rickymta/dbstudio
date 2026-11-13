import i18n from 'i18next';
import { initReactI18next } from 'react-i18next';
import en from '../locales/en.json';
import vi from '../locales/vi.json';

export const LANGUAGES = ['vi', 'en'] as const;
export type Language = (typeof LANGUAGES)[number];

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
  resources: { en: { translation: en }, vi: { translation: vi } },
  lng: detectLanguage(),
  fallbackLng: 'en',
  interpolation: { escapeValue: false },
});

i18n.on('languageChanged', (lng) => {
  document.documentElement.lang = lng;
});
document.documentElement.lang = i18n.language;

export default i18n;

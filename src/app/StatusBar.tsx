import { useQuery } from '@tanstack/react-query';
import { Languages, Monitor, Moon, Sun } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Tooltip } from '@/components/ui/Tooltip';
import { ipc } from '@/lib/ipc';
import { setLanguage, type Language } from '@/lib/i18n';
import { THEME_MODES, useThemeStore, type ThemeMode } from '@/stores/theme';

const themeIcons: Record<ThemeMode, typeof Sun> = { system: Monitor, light: Sun, dark: Moon };

export function StatusBar() {
  const { t, i18n } = useTranslation();
  const { data: info } = useQuery({ queryKey: ['appInfo'], queryFn: ipc.appInfo });
  const mode = useThemeStore((s) => s.mode);
  const setMode = useThemeStore((s) => s.setMode);

  const nextMode = THEME_MODES[(THEME_MODES.indexOf(mode) + 1) % THEME_MODES.length] ?? 'system';
  const ThemeIcon = themeIcons[mode];
  const language: Language = i18n.language.startsWith('vi') ? 'vi' : 'en';
  const nextLanguage: Language = language === 'vi' ? 'en' : 'vi';

  return (
    <footer className="flex h-7 shrink-0 items-center justify-between border-t border-border bg-surface px-3 text-xs text-muted-foreground">
      <span data-testid="app-version">
        {info ? `${info.name} v${info.version}` : t('app.name')}
        {info && info.os !== 'browser' ? ` · ${info.os}/${info.arch}` : ''}
      </span>
      <div className="flex items-center gap-1">
        <Tooltip content={`${t('theme.label')}: ${t(`theme.${mode}`)}`}>
          <Button
            variant="ghost"
            size="sm"
            aria-label={`${t('theme.label')}: ${t(`theme.${mode}`)}`}
            onClick={() => setMode(nextMode)}
          >
            <ThemeIcon size={14} />
            <span>{t(`theme.${mode}`)}</span>
          </Button>
        </Tooltip>
        <Tooltip content={t('language.label')}>
          <Button
            variant="ghost"
            size="sm"
            aria-label={`${t('language.label')}: ${t(`language.${language}`)}`}
            onClick={() => setLanguage(nextLanguage)}
          >
            <Languages size={14} />
            <span>{language.toUpperCase()}</span>
          </Button>
        </Tooltip>
      </div>
    </footer>
  );
}

import { useQuery } from '@tanstack/react-query';
import { Languages, Lock, Monitor, Moon, Settings, Sun } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Tooltip } from '@/components/ui/Tooltip';
import { connectionColor, useConnectionProfile } from '@/features/connections/queries';
import { StatusTransaction } from '@/features/editor/StatusTransaction';
import { SettingsDialog } from '@/features/settings/SettingsDialog';
import { ipc, ipcTransport } from '@/lib/ipc';
import { setLanguage, type Language } from '@/lib/i18n';
import { useSessionsStore } from '@/stores/sessions';
import { useTabsStore } from '@/stores/tabs';
import { StatusBarSlot } from './StatusBarSlot';
import { THEME_MODES, useThemeStore, type ThemeMode } from '@/stores/theme';

const themeIcons: Record<ThemeMode, typeof Sun> = { system: Monitor, light: Sun, dark: Moon };

/** Connection of the active tab: name, environment color and label, read-only lock (CONN-03). */
function ActiveConnection() {
  const { t } = useTranslation();
  const connectionId = useTabsStore(
    (s) => s.tabs.find((x) => x.id === s.activeId)?.payload?.connectionId,
  );
  const profile = useConnectionProfile(connectionId ?? '');
  // TiDB has no server-side read-only session: only DBStudio's statement guard blocks writes.
  const appOnly = useSessionsStore(
    (s) =>
      s.sessionsByConnection[connectionId ?? '']?.serverInfo.extra?.readOnlyEnforcement ===
      'application',
  );
  if (!connectionId || !profile) return null;
  const color = connectionColor(profile);
  return (
    <span
      data-testid="status-connection"
      className="flex min-w-0 items-center gap-1.5 border-l border-border pl-3"
    >
      <span
        data-testid="status-connection-color"
        aria-hidden
        className="size-2 shrink-0 rounded-full bg-border"
        style={color ? { backgroundColor: color } : undefined}
      />
      <span className="truncate text-foreground" title={profile.name}>
        {profile.name}
      </span>
      {profile.environment && (
        <span className="shrink-0">{t(`connections:environment.${profile.environment}`)}</span>
      )}
      {profile.readOnly && (
        <Lock size={11} aria-label={t('connections:row.readOnly')} className="shrink-0" />
      )}
      {profile.readOnly && appOnly && (
        <span
          data-testid="status-read-only-app"
          className="shrink-0 text-warning"
          title={t('connections:row.readOnlyAppOnlyHint')}
        >
          {t('connections:row.readOnlyAppOnly')}
        </span>
      )}
    </span>
  );
}

export function StatusBar() {
  const { t, i18n } = useTranslation();
  const { data: info } = useQuery({ queryKey: ['appInfo'], queryFn: ipc.appInfo });
  const [settingsOpen, setSettingsOpen] = useState(false);
  const transport = ipcTransport();
  const mode = useThemeStore((s) => s.mode);
  const setMode = useThemeStore((s) => s.setMode);

  // Cmd/Ctrl+, opens the settings.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && !e.altKey && e.key === ',') {
        e.preventDefault();
        setSettingsOpen(true);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  const nextMode = THEME_MODES[(THEME_MODES.indexOf(mode) + 1) % THEME_MODES.length] ?? 'system';
  const ThemeIcon = themeIcons[mode];
  const language: Language = i18n.language.startsWith('vi') ? 'vi' : 'en';
  const nextLanguage: Language = language === 'vi' ? 'en' : 'vi';

  return (
    <footer className="flex min-h-9 shrink-0 items-center justify-between border-t border-border bg-surface px-3 text-xs text-muted-foreground">
      <div className="flex min-w-0 items-center gap-2">
        <span data-testid="app-version" className="shrink-0">
          {info ? `${info.name} v${info.version}` : t('app.name')}
          {info && info.os !== 'browser' ? ` · ${info.os}/${info.arch}` : ''}
        </span>
        {transport !== 'tauri' && (
          <span
            data-testid="ipc-transport"
            title={t('transport.hint', { transport })}
            className="shrink-0 rounded bg-warning-soft px-1.5 py-px text-[11px] font-medium text-warning"
          >
            {transport}
          </span>
        )}
        <ActiveConnection />
        <StatusTransaction />
        <StatusBarSlot name="jobs" />
      </div>
      <div className="flex items-center gap-1">
        <Tooltip content={t('settings:open')}>
          <Button
            variant="ghost"
            size="sm"
            aria-label={t('settings:open')}
            aria-keyshortcuts="Control+, Meta+,"
            onClick={() => setSettingsOpen(true)}
          >
            <Settings size={14} />
          </Button>
        </Tooltip>
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
      <SettingsDialog open={settingsOpen} onOpenChange={setSettingsOpen} />
    </footer>
  );
}

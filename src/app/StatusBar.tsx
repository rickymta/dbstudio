import { useQuery } from '@tanstack/react-query';
import {
  CircleHelp,
  Keyboard,
  Languages,
  ListChecks,
  Lock,
  Minus,
  Monitor,
  Moon,
  Plus,
  Settings,
  Sun,
} from 'lucide-react';
import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Tooltip } from '@/components/ui/Tooltip';
import { connectionColor, useConnectionProfile } from '@/features/connections/queries';
import { StatusTransaction } from '@/features/editor/StatusTransaction';
import { SettingsDialog } from '@/features/settings/SettingsDialog';
import { formatKeys, keysOf } from '@/features/editor/shortcuts';
import { useShortcutsHelp } from '@/features/editor/shortcutsHelpStore';
import {
  useActivityWindow,
  useHelpDialog,
  useSettingsDialog,
} from '@/features/workspace/paletteStore';
import { ZOOM_STEP, useZoom } from '@/features/workspace/zoom';
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
  // Server the session landed on (a host list, Sentinel, Redis Cluster): drivers report it.
  const extra = useSessionsStore(
    (s) => s.sessionsByConnection[connectionId ?? '']?.serverInfo.extra,
  );
  if (!connectionId || !profile) return null;
  const color = connectionColor(profile);
  const clustered =
    !!profile.options.hosts?.trim() ||
    ['sentinel', 'cluster'].includes((profile.options.topology ?? '').toLowerCase());
  const node = clustered ? extra?.node : undefined;
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
      {node && (
        <span
          data-testid="status-node"
          className="min-w-0 truncate"
          title={t('connections:nodeStatus.title', { node })}
        >
          {extra?.clusterPrimaries
            ? t('connections:nodeStatus.cluster', { count: Number(extra.clusterPrimaries) })
            : node}
          {extra?.nodeRole && ` · ${t(`connections:nodeStatus.role.${extra.nodeRole}`)}`}
        </span>
      )}
    </span>
  );
}

export function StatusBar() {
  const { t, i18n } = useTranslation();
  const { data: info } = useQuery({ queryKey: ['appInfo'], queryFn: ipc.appInfo });
  const settingsOpen = useSettingsDialog((s) => s.open);
  const setSettingsOpen = useSettingsDialog((s) => s.setOpen);
  const zoom = useZoom((s) => s.zoom);
  const setZoom = useZoom((s) => s.setZoom);
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
  }, [setSettingsOpen]);

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
        <Tooltip content={`${t('activity.title')} (${formatKeys(keysOf('activity'))})`}>
          <Button
            variant="ghost"
            size="sm"
            aria-label={t('activity.title')}
            onClick={() => useActivityWindow.getState().setOpen(true)}
          >
            <ListChecks size={14} />
          </Button>
        </Tooltip>
        <Tooltip content={`${t('editor:shortcuts.title')} (${formatKeys(keysOf('help'))})`}>
          <Button
            variant="ghost"
            size="sm"
            aria-label={t('editor:shortcuts.title')}
            onClick={() => useShortcutsHelp.getState().setOpen(true)}
          >
            <Keyboard size={14} />
          </Button>
        </Tooltip>
        <span className="flex items-center" data-testid="zoom-control">
          <Button
            variant="ghost"
            size="sm"
            aria-label={t('zoom.out')}
            onClick={() => setZoom(zoom - ZOOM_STEP)}
          >
            <Minus size={12} />
          </Button>
          <Tooltip content={`${t('zoom.reset')} (${formatKeys(keysOf('zoomReset'))})`}>
            <Button
              variant="ghost"
              size="sm"
              aria-label={t('zoom.reset')}
              className="tabular-nums"
              onClick={() => setZoom(1)}
            >
              {`${Math.round(zoom * 100)}%`}
            </Button>
          </Tooltip>
          <Button
            variant="ghost"
            size="sm"
            aria-label={t('zoom.in')}
            onClick={() => setZoom(zoom + ZOOM_STEP)}
          >
            <Plus size={12} />
          </Button>
        </span>
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
        <Tooltip content={t('help:title')}>
          <Button
            variant="ghost"
            size="sm"
            aria-label={t('help:title')}
            onClick={() => useHelpDialog.getState().setOpen(true)}
          >
            <CircleHelp size={14} />
            <span>{t('help:open')}</span>
          </Button>
        </Tooltip>
      </div>
      <SettingsDialog open={settingsOpen} onOpenChange={setSettingsOpen} />
    </footer>
  );
}

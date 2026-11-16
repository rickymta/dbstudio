import { ChevronDown, Lock, Plug } from 'lucide-react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/DropdownMenu';
import { useToastStore } from '@/components/ui/toastStore';
import { EnvironmentBadge } from '@/features/connections/ConnectionContext';
import { connectionColor, useConnectionsSnapshot } from '@/features/connections/queries';
import { useSessionOpen } from '@/features/connections/useSessionOpen';
import { toIpcError } from '@/lib/ipc';
import type { ConnectionProfile } from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { editorTabId, useTabsStore, type EditorMode } from '@/stores/tabs';
import {
  filterKeyDown,
  groupConnections,
  rebindEditor,
  switchBlock,
  useSwitchBlock,
} from './pickerLogic';

function Dot({ profile }: { profile: ConnectionProfile }) {
  const color = connectionColor(profile);
  return (
    <span
      aria-hidden
      className="size-2.5 shrink-0 rounded-full bg-border"
      style={color ? { backgroundColor: color } : undefined}
    />
  );
}

/**
 * "Active connection" of an editor, like DBeaver's: shows the connection the editor runs on and
 * lets the user switch it to another open (or saved) connection of the same kind of editor.
 */
export function ConnectionPicker({
  tabStateId,
  connectionId,
  mode,
}: {
  tabStateId: string;
  connectionId: string | null;
  mode: EditorMode;
}) {
  const { t } = useTranslation('editor');
  const { t: tc } = useTranslation('connections');
  const { data: snapshot } = useConnectionsSnapshot();
  const sessions = useSessionsStore((s) => s.sessionsByConnection);
  const { open: openSession, dialog: openDialog } = useSessionOpen();
  const [filter, setFilter] = useState('');
  const toast = useToastStore.getState().push;

  const profiles = snapshot?.profiles ?? [];
  const current = profiles.find((p) => p.id === connectionId);
  const groups = groupConnections(profiles, sessions, mode, filter);
  const blocked = useSwitchBlock(tabStateId);

  const choose = async (target: string) => {
    if (target === connectionId) return;
    const block = switchBlock(tabStateId);
    if (block) return void toast('info', t(`picker.${block}`));
    const result = await rebindEditor(tabStateId, target);
    if (!result || result.session) return;
    const tabId = editorTabId(tabStateId);
    try {
      const session = await openSession(target);
      // Keep the payload's session in step (session guards look at it), unless the user moved on.
      const tab = useTabsStore.getState().tabs.find((x) => x.id === tabId);
      if (tab?.kind === 'editor' && tab.payload.connectionId === target) {
        useTabsStore
          .getState()
          .rebindEditor(tabId, { connectionId: target, sessionId: session.sessionId });
      }
    } catch (e) {
      // The tab stays on the new connection, not connected: its "Connect" button retries.
      if (toIpcError(e).kind === 'cancelled') return;
      toast('error', t('picker.connectFailed', { message: toIpcError(e).message }));
    }
  };

  const item = (p: ConnectionProfile) => (
    <DropdownMenuRadioItem key={p.id} value={p.id} title={`${p.name} · ${p.host}`}>
      <Dot profile={p} />
      <span className="min-w-0 truncate">{p.name}</span>
      {p.environment && <EnvironmentBadge environment={p.environment} />}
    </DropdownMenuRadioItem>
  );

  return (
    <>
      {openDialog}
      <DropdownMenu onOpenChange={(open) => open && setFilter('')}>
        <DropdownMenuTrigger
          disabled={blocked !== null}
          aria-label={t('picker.connection')}
          title={blocked ? t(`picker.${blocked}`) : t('picker.connection')}
          data-testid="connection-picker"
          className="flex min-w-0 items-center gap-2 rounded px-1.5 py-1 text-[13px] hover:bg-surface-muted disabled:cursor-not-allowed disabled:opacity-60"
        >
          {current ? (
            <>
              <Dot profile={current} />
              <span className="max-w-60 truncate font-medium">{current.name}</span>
              {current.environment && <EnvironmentBadge environment={current.environment} />}
              {current.readOnly && (
                <span className="flex shrink-0 items-center gap-1 text-xs text-muted-foreground">
                  <Lock size={13} aria-hidden />
                  {tc('row.readOnly')}
                </span>
              )}
            </>
          ) : (
            <>
              <Plug size={13} aria-hidden className="shrink-0 text-muted-foreground" />
              <span className="text-muted-foreground">{t('picker.choose')}</span>
            </>
          )}
          <ChevronDown size={13} aria-hidden className="shrink-0 text-muted-foreground" />
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start" className="max-h-80 w-72 overflow-y-auto">
          <input
            type="search"
            autoFocus
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            onKeyDown={filterKeyDown}
            aria-label={t('picker.filter')}
            placeholder={t('picker.filter')}
            className="mb-1 h-7 w-full rounded border border-border bg-surface px-2 text-[13px] outline-none focus:border-accent"
          />
          {groups.open.length + groups.other.length === 0 && (
            <div className="px-2 py-1.5 text-xs text-muted-foreground">{t('picker.none')}</div>
          )}
          <DropdownMenuRadioGroup value={connectionId ?? ''} onValueChange={(v) => void choose(v)}>
            {groups.open.length > 0 && (
              <>
                <DropdownMenuLabel>{t('picker.open')}</DropdownMenuLabel>
                {groups.open.map(item)}
              </>
            )}
            {groups.open.length > 0 && groups.other.length > 0 && <DropdownMenuSeparator />}
            {groups.other.length > 0 && (
              <>
                <DropdownMenuLabel>{t('picker.other')}</DropdownMenuLabel>
                {groups.other.map(item)}
              </>
            )}
          </DropdownMenuRadioGroup>
        </DropdownMenuContent>
      </DropdownMenu>
    </>
  );
}

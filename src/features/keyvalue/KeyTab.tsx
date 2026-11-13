import { AlertTriangle, KeyRound, Search } from 'lucide-react';
import { useCallback, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { ConfirmDialog } from '@/components/ui/ConfirmDialog';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { EmptyState } from '@/components/ui/EmptyState';
import { Input } from '@/components/ui/Input';
import { Spinner } from '@/components/ui/Spinner';
import { toast } from '@/components/ui/toastStore';
import { useConnectionProfile } from '@/features/connections/queries';
import { toIpcError } from '@/lib/ipc';
import type { KeyChange } from '@/lib/types';
import { keyTabId, useTabsStore, type KeyTabPayload } from '@/stores/tabs';
import { HashView, ListView, SetView, StreamView, ZSetView } from './EntryViews';
import { KeyHeader } from './KeyHeader';
import { keyNameOfPath } from './keyDetail';
import {
  buildKeyChangeSet,
  deleteKeyChange,
  entryCount,
  expireChange,
  renameChange,
} from './keyChanges';
import { applyKeyChangeSet, changeFailure, previewKeyChangeSet } from './ipcKeys';
import { StringEditor } from './StringEditor';
import { useKeyData } from './useKeyData';

type Dialogs = 'ttl' | 'rename' | 'delete' | 'preview' | null;

/** Viewer/editor of one Redis key: header, per-type body and the pending change list. */
export function KeyTab({ sessionId, connectionId, path }: KeyTabPayload) {
  const { t } = useTranslation('keyvalue');
  const keyName = keyNameOfPath(path);
  const profile = useConnectionProfile(connectionId);
  const readOnly = profile?.readOnly ?? false;
  const { data, reload, loadMore } = useKeyData(sessionId, path);
  const [pending, setPending] = useState<KeyChange[]>([]);
  const [dialog, setDialog] = useState<Dialogs>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [previewLines, setPreviewLines] = useState<string[] | null>(null);
  const [patternDraft, setPatternDraft] = useState('');
  const closeTab = useTabsStore((s) => s.closeTab);
  const openTab = useTabsStore((s) => s.openTab);

  const stage = useCallback((change: KeyChange) => {
    setNotice(null);
    setPending((p) =>
      // Only the latest whole-value save matters.
      change.op === 'setString'
        ? [...p.filter((c) => c.op !== 'setString'), change]
        : [...p, change],
    );
  }, []);

  /** Apply changes; `fromPending` drops the applied prefix of the pending list afterwards. */
  const apply = useCallback(
    async (changes: KeyChange[], fromPending: boolean): Promise<boolean> => {
      setBusy(true);
      setNotice(null);
      try {
        const result = await applyKeyChangeSet(sessionId, buildKeyChangeSet(path, changes));
        if (fromPending) setPending([]);
        toast.success(t('apply.done', { count: result.applied }));
        return true;
      } catch (e) {
        const failure = changeFailure(e);
        if (failure) {
          // Redis has no rollback: the changes before the failing one are already in place.
          if (fromPending) setPending((p) => p.slice(failure.applied));
          setNotice(
            t('apply.failed', {
              index: failure.index + 1,
              applied: failure.applied,
              message: failure.error.message,
            }),
          );
        } else {
          setNotice(toIpcError(e).message);
        }
        return false;
      } finally {
        setBusy(false);
        void reload();
      }
    },
    [sessionId, path, reload, t],
  );

  const openPreview = async () => {
    setBusy(true);
    try {
      setPreviewLines(await previewKeyChangeSet(sessionId, buildKeyChangeSet(path, pending)));
      setDialog('preview');
    } catch (e) {
      setNotice(toIpcError(e).message);
    } finally {
      setBusy(false);
    }
  };

  const onSearch = (e: FormEvent) => {
    e.preventDefault();
    void reload(patternDraft.trim());
  };

  const { page, entries } = data;
  const searchable =
    entries?.type === 'hash' || entries?.type === 'set' || entries?.type === 'zSet';

  let body = null;
  if (data.status === 'loading' && !page) {
    body = (
      <div className="flex h-full items-center justify-center">
        <Spinner size={18} />
      </div>
    );
  } else if (data.status === 'missing') {
    body = (
      <EmptyState
        icon={<KeyRound size={32} />}
        title={t('missing.title')}
        description={t('missing.description')}
      />
    );
  } else if (data.status === 'error' && !page) {
    body = (
      <EmptyState
        icon={<AlertTriangle size={32} />}
        title={t('error.title')}
        description={data.error?.message}
        action={<Button onClick={() => void reload()}>{t('actions.retry')}</Button>}
      />
    );
  } else if (entries) {
    const common = { keyName, readOnly, stage };
    switch (entries.type) {
      case 'string':
        body = (
          <StringEditor
            key={data.fetchedAt}
            keyName={keyName}
            value={entries.value}
            totalBytes={entries.totalBytes}
            truncated={entries.truncated}
            readOnly={readOnly}
            stage={stage}
          />
        );
        break;
      case 'hash':
        body = <HashView {...common} entries={entries} />;
        break;
      case 'list':
        body = <ListView {...common} entries={entries} />;
        break;
      case 'set':
        body = <SetView {...common} entries={entries} />;
        break;
      case 'zSet':
        body = <ZSetView {...common} entries={entries} />;
        break;
      case 'stream':
        body = <StreamView {...common} entries={entries} />;
        break;
      case 'json':
        body = (
          <div className="flex h-full min-h-0 flex-col gap-2 p-2">
            <p className="text-xs text-muted-foreground">{t('json.readOnly')}</p>
            <pre
              data-testid="key-json"
              className="min-h-0 flex-1 select-text overflow-auto rounded border border-border p-2 font-mono text-[12px]"
            >
              {entries.text}
            </pre>
          </div>
        );
        break;
      case 'unsupported':
        body = (
          <EmptyState
            icon={<AlertTriangle size={32} />}
            title={t('unsupported.title', { type: entries.typeName })}
            description={t('unsupported.description')}
          />
        );
        break;
    }
  }

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="key-tab">
      <KeyHeader
        name={keyName}
        page={page}
        fetchedAt={data.fetchedAt}
        readOnly={readOnly}
        busy={busy}
        onRefresh={() => void reload()}
        onTtl={() => setDialog('ttl')}
        onRename={() => setDialog('rename')}
        onDelete={() => setDialog('delete')}
      />
      {notice && (
        <p
          role="alert"
          className="border-b border-border bg-danger/10 px-3 py-1.5 text-xs text-danger"
        >
          {notice}
        </p>
      )}
      {searchable && (
        <form
          onSubmit={onSearch}
          className="flex items-center gap-2 border-b border-border px-2 py-1.5"
        >
          <Search size={13} className="text-muted-foreground" aria-hidden />
          <Input
            aria-label={t('search.label')}
            placeholder={t('search.placeholder')}
            value={patternDraft}
            onChange={(e) => setPatternDraft(e.target.value)}
          />
          <Button type="submit" size="sm">
            {t('search.apply')}
          </Button>
          {data.pattern && (
            <Button
              size="sm"
              onClick={() => {
                setPatternDraft('');
                void reload('');
              }}
            >
              {t('search.clear')}
            </Button>
          )}
        </form>
      )}
      <div className="min-h-0 flex-1 overflow-auto">{body}</div>
      {entries && data.nextCursor && (
        <div className="flex items-center justify-between border-t border-border px-3 py-1 text-xs">
          <span className="text-muted-foreground">
            {t('loaded', { loaded: entryCount(entries), total: page?.length ?? '?' })}
          </span>
          <Button size="sm" disabled={data.loadingMore} onClick={() => void loadMore()}>
            {data.loadingMore && <Spinner size={11} />} {t('loadMore')}
          </Button>
        </div>
      )}
      {pending.length > 0 && (
        <div
          className="flex items-center gap-2 border-t border-border bg-surface-muted px-3 py-2 text-xs"
          data-testid="pending-bar"
        >
          <span className="font-medium">{t('pending.count', { count: pending.length })}</span>
          <span className="text-muted-foreground">{t('pending.noRollback')}</span>
          <div className="ml-auto flex gap-2">
            <Button size="sm" onClick={() => setPending([])} disabled={busy}>
              {t('pending.discard')}
            </Button>
            <Button size="sm" onClick={() => void openPreview()} disabled={busy}>
              {t('pending.preview')}
            </Button>
            <Button
              size="sm"
              variant="primary"
              onClick={() => void apply(pending, true)}
              disabled={busy || readOnly}
            >
              {t('pending.apply')}
            </Button>
          </div>
        </div>
      )}

      <Dialog open={dialog === 'preview'} onOpenChange={(open) => !open && setDialog(null)}>
        <DialogContent
          title={t('preview.title')}
          description={t('preview.description')}
          footer={
            <>
              <Button onClick={() => setDialog(null)}>{t('actions.close')}</Button>
              <Button
                variant="primary"
                disabled={busy || readOnly}
                onClick={() => {
                  setDialog(null);
                  void apply(pending, true);
                }}
              >
                {t('pending.apply')}
              </Button>
            </>
          }
        >
          <pre
            data-testid="preview-commands"
            className="select-text overflow-auto font-mono text-[12px] whitespace-pre-wrap"
          >
            {(previewLines ?? []).join('\n')}
          </pre>
        </DialogContent>
      </Dialog>

      {dialog === 'ttl' && (
        <TtlDialog
          currentMs={page?.ttlMs ?? null}
          onClose={() => setDialog(null)}
          onSubmit={async (ttlMs) => {
            setDialog(null);
            await apply([expireChange(keyName, ttlMs)], false);
          }}
        />
      )}
      {dialog === 'rename' && (
        <RenameDialog
          current={keyName}
          onClose={() => setDialog(null)}
          onSubmit={async (newName) => {
            setDialog(null);
            if (await apply([renameChange(keyName, newName)], false)) {
              const id = keyTabId(sessionId, path);
              const newPath = {
                segments: [
                  ...path.segments.filter((s) => s.kind === 'database'),
                  { kind: 'key' as const, name: newName },
                ],
              };
              closeTab(id);
              openTab({
                id: keyTabId(sessionId, newPath),
                kind: 'key',
                title: newName,
                closable: true,
                payload: { sessionId, connectionId, path: newPath },
              });
            }
          }}
        />
      )}
      <ConfirmDialog
        open={dialog === 'delete'}
        onOpenChange={(open) => !open && setDialog(null)}
        title={t('delete.title')}
        description={t('delete.description', { name: keyName })}
        confirmLabel={t('actions.deleteKey')}
        destructive
        onConfirm={() => {
          setPending([]);
          void apply([deleteKeyChange(keyName)], false);
        }}
      />
    </div>
  );
}

function TtlDialog({
  currentMs,
  onClose,
  onSubmit,
}: {
  currentMs: number | null;
  onClose: () => void;
  onSubmit: (ttlMs: number | null) => void;
}) {
  const { t } = useTranslation('keyvalue');
  const [seconds, setSeconds] = useState(
    currentMs === null ? '' : String(Math.max(1, Math.round(currentMs / 1000))),
  );
  const n = Number(seconds);
  // PEXPIRE with 0 ms would delete the key: at least one millisecond is required.
  const valid = seconds.trim() !== '' && Number.isFinite(n) && Math.round(n * 1000) >= 1;
  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent
        title={t('ttl.title')}
        description={t('ttl.description')}
        className="w-[min(420px,92vw)]"
        footer={
          <>
            <Button onClick={() => onSubmit(null)} disabled={currentMs === null}>
              {t('ttl.persist')}
            </Button>
            <Button
              variant="primary"
              disabled={!valid}
              onClick={() => onSubmit(Math.round(n * 1000))}
            >
              {t('ttl.set')}
            </Button>
          </>
        }
      >
        <Input
          autoFocus
          type="number"
          min={1}
          aria-label={t('ttl.seconds')}
          value={seconds}
          onChange={(e) => setSeconds(e.target.value)}
        />
      </DialogContent>
    </Dialog>
  );
}

function RenameDialog({
  current,
  onClose,
  onSubmit,
}: {
  current: string;
  onClose: () => void;
  onSubmit: (newName: string) => void;
}) {
  const { t } = useTranslation('keyvalue');
  const [name, setName] = useState(current);
  const valid = name !== '' && name !== current;
  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent
        title={t('rename.title')}
        className="w-[min(420px,92vw)]"
        footer={
          <Button variant="primary" disabled={!valid} onClick={() => onSubmit(name)}>
            {t('rename.apply')}
          </Button>
        }
      >
        <Input
          autoFocus
          aria-label={t('rename.newName')}
          value={name}
          className="font-mono"
          onChange={(e) => setName(e.target.value)}
        />
      </DialogContent>
    </Dialog>
  );
}

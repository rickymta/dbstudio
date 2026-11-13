// Query history (QUERY-07) and saved queries / snippets (QUERY-08), as a drawer of the editor.

import { useInfiniteQuery, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  CircleAlert,
  CircleCheck,
  CircleSlash,
  Clock,
  FolderPlus,
  Plus,
  Save,
  Trash2,
} from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { ConfirmDialog } from '@/components/ui/ConfirmDialog';
import { Checkbox } from '@/components/ui/Checkbox';
import { IconButton } from '@/components/ui/IconButton';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';
import { Select } from '@/components/ui/Select';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/Tabs';
import { Spinner } from '@/components/ui/Spinner';
import { ipc } from '@/lib/ipc';
import type { DbKind, HistoryEntry, SavedFolder, SavedQuery, Uuid } from '@/lib/types';

const PAGE = 50;
const NO_FOLDER = '__none__';

export interface SidePanelProps {
  connectionId: Uuid | null;
  dbKind: DbKind | null;
  /** Text to save as a query (the selection, else the whole editor). */
  currentText: () => string;
  /** Bump to refresh the history after a run. */
  runId: number | null;
  onInsert: (text: string, snippet: boolean) => void;
  onOpenInNewTab: (text: string) => void;
}

export function SidePanel(props: SidePanelProps) {
  const { t } = useTranslation('editor');
  return (
    <aside
      className="flex h-full w-80 shrink-0 flex-col border-l border-border bg-surface"
      data-testid="side-panel"
    >
      <Tabs defaultValue="history" className="flex h-full min-h-0 flex-col">
        <TabsList>
          <TabsTrigger value="history">{t('history.title')}</TabsTrigger>
          <TabsTrigger value="saved">{t('saved.title')}</TabsTrigger>
        </TabsList>
        <TabsContent value="history" className="min-h-0 flex-1 overflow-hidden">
          <HistoryList {...props} />
        </TabsContent>
        <TabsContent value="saved" className="min-h-0 flex-1 overflow-hidden">
          <SavedList {...props} />
        </TabsContent>
      </Tabs>
    </aside>
  );
}

function useDebounced<T>(value: T, ms: number): T {
  const [v, setV] = useState(value);
  useEffect(() => {
    const id = setTimeout(() => setV(value), ms);
    return () => clearTimeout(id);
  }, [value, ms]);
  return v;
}

const STATUS_ICON: Record<string, typeof CircleCheck> = {
  ok: CircleCheck,
  error: CircleAlert,
  cancelled: CircleSlash,
  running: Clock,
};

export function HistoryList({ connectionId, runId, onInsert, onOpenInNewTab }: SidePanelProps) {
  const { t, i18n } = useTranslation('editor');
  const qc = useQueryClient();
  const [search, setSearch] = useState('');
  const [status, setStatus] = useState('all');
  const [confirmClear, setConfirmClear] = useState(false);
  const term = useDebounced(search.trim(), 250);
  const key = ['editorHistory', connectionId, term, status, runId] as const;
  const query = useInfiniteQuery({
    queryKey: key,
    initialPageParam: null as number | null,
    queryFn: ({ pageParam }) =>
      ipc.historyList({
        connectionId,
        search: term || null,
        status: status === 'all' ? null : status,
        beforeId: pageParam,
        limit: PAGE,
      }),
    getNextPageParam: (last: HistoryEntry[]) =>
      last.length === PAGE ? last[last.length - 1]!.id : undefined,
  });
  const entries = query.data?.pages.flat() ?? [];
  const refresh = () => qc.invalidateQueries({ queryKey: ['editorHistory'] });
  const df = new Intl.DateTimeFormat(i18n.language, { dateStyle: 'short', timeStyle: 'medium' });

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="history-list">
      <div className="flex flex-col gap-1.5 border-b border-border p-2">
        <Input
          type="search"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          placeholder={t('history.search')}
          aria-label={t('history.search')}
        />
        <div className="flex items-center gap-2">
          <Select
            aria-label={t('history.statusFilter')}
            value={status}
            onValueChange={setStatus}
            options={[
              { value: 'all', label: t('history.status.all') },
              { value: 'ok', label: t('history.status.ok') },
              { value: 'error', label: t('history.status.error') },
              { value: 'cancelled', label: t('history.status.cancelled') },
            ]}
          />
          <IconButton
            label={t('history.clear')}
            icon={<Trash2 size={13} />}
            disabled={entries.length === 0}
            onClick={() => setConfirmClear(true)}
          />
        </div>
      </div>
      <ul className="min-h-0 flex-1 overflow-auto">
        {query.isPending && (
          <li className="flex justify-center p-4">
            <Spinner />
          </li>
        )}
        {!query.isPending && entries.length === 0 && (
          <li className="p-4 text-center text-xs text-muted-foreground">{t('history.empty')}</li>
        )}
        {entries.map((h) => {
          const Icon = STATUS_ICON[h.status] ?? Clock;
          return (
            <li
              key={h.id}
              className="group border-b border-border px-2 py-1.5"
              data-testid="history-entry"
            >
              <div className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
                <Icon
                  size={12}
                  aria-label={t(`history.status.${h.status}`, { defaultValue: h.status })}
                  className={
                    h.status === 'error'
                      ? 'text-danger'
                      : h.status === 'ok'
                        ? 'text-success'
                        : undefined
                  }
                />
                <time dateTime={h.startedAt}>{df.format(new Date(h.startedAt))}</time>
                {h.elapsedMs !== null && <span>{t('results.elapsed', { ms: h.elapsedMs })}</span>}
                {h.rows !== null && <span>{t('history.rows', { count: h.rows })}</span>}
                <span className="ml-auto flex opacity-0 group-focus-within:opacity-100 group-hover:opacity-100">
                  <IconButton
                    label={t('history.delete')}
                    icon={<Trash2 size={12} />}
                    onClick={() => void ipc.historyDelete([h.id]).then(refresh)}
                  />
                </span>
              </div>
              <button
                type="button"
                className="mt-0.5 block w-full text-left font-mono text-xs break-all whitespace-pre-wrap text-foreground hover:text-accent"
                title={t('history.reopen')}
                onClick={() => onInsert(h.text, false)}
                onDoubleClick={() => onOpenInNewTab(h.text)}
              >
                {h.text.length > 240 ? `${h.text.slice(0, 240)}…` : h.text}
              </button>
              {h.errorMessage && (
                <p className="mt-0.5 text-[11px] break-words text-danger">{h.errorMessage}</p>
              )}
            </li>
          );
        })}
        {query.hasNextPage && (
          <li className="p-2 text-center">
            <Button
              size="sm"
              disabled={query.isFetchingNextPage}
              onClick={() => void query.fetchNextPage()}
            >
              {t('history.loadMore')}
            </Button>
          </li>
        )}
      </ul>
      <ConfirmDialog
        open={confirmClear}
        onOpenChange={setConfirmClear}
        destructive
        title={t('history.clearTitle')}
        description={t('history.clearDescription')}
        confirmLabel={t('history.clear')}
        onConfirm={() => void ipc.historyClear(connectionId).then(refresh)}
      />
    </div>
  );
}

const savedKey = ['savedQueries'] as const;

export function SavedList({ connectionId, dbKind, currentText, onInsert }: SidePanelProps) {
  const { t } = useTranslation('editor');
  const qc = useQueryClient();
  const { data } = useQuery({ queryKey: savedKey, queryFn: ipc.savedQueriesList });
  const [name, setName] = useState('');
  const [folderId, setFolderId] = useState(NO_FOLDER);
  const [snippet, setSnippet] = useState(false);
  const [newFolder, setNewFolder] = useState('');
  const refresh = () => qc.invalidateQueries({ queryKey: savedKey });

  const folders = data?.folders ?? [];
  const visible = (data?.queries ?? [])
    .filter((q) => q.connectionId === null || q.connectionId === connectionId)
    .filter((q) => q.dbKind === null || q.dbKind === dbKind)
    .sort((a, b) => a.sortOrder - b.sortOrder || a.name.localeCompare(b.name));

  const save = async () => {
    const text = currentText();
    if (!name.trim() || text.trim() === '') return;
    const now = new Date().toISOString();
    const query: SavedQuery = {
      id: crypto.randomUUID(),
      folderId: folderId === NO_FOLDER ? null : folderId,
      name: name.trim(),
      text,
      isSnippet: snippet,
      dbKind,
      connectionId: null,
      description: null,
      sortOrder: (data?.queries.length ?? 0) + 1,
      createdAt: now,
      updatedAt: now,
    };
    await ipc.savedQuerySave(query);
    setName('');
    await refresh();
  };
  const addFolder = async () => {
    if (!newFolder.trim()) return;
    const folder: SavedFolder = {
      id: crypto.randomUUID(),
      name: newFolder.trim(),
      parentId: null,
      sortOrder: folders.length + 1,
    };
    await ipc.savedFolderSave(folder);
    setNewFolder('');
    await refresh();
  };

  const groups: { folder: SavedFolder | null; queries: SavedQuery[] }[] = [
    {
      folder: null,
      queries: visible.filter(
        (q) => q.folderId === null || !folders.some((f) => f.id === q.folderId),
      ),
    },
    ...[...folders]
      .sort((a, b) => a.sortOrder - b.sortOrder || a.name.localeCompare(b.name))
      .map((f) => ({ folder: f, queries: visible.filter((q) => q.folderId === f.id) })),
  ];

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="saved-list">
      <div className="flex flex-col gap-1.5 border-b border-border p-2">
        <Label htmlFor="saved-name">{t('saved.saveCurrent')}</Label>
        <Input
          id="saved-name"
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder={t('saved.name')}
          onKeyDown={(e) => e.key === 'Enter' && void save()}
        />
        <div className="flex items-center gap-2">
          <Select
            aria-label={t('saved.folder')}
            value={folderId}
            onValueChange={setFolderId}
            options={[
              { value: NO_FOLDER, label: t('saved.noFolder') },
              ...folders.map((f) => ({ value: f.id, label: f.name })),
            ]}
          />
          <label className="flex shrink-0 items-center gap-1 text-xs">
            <Checkbox
              checked={snippet}
              onCheckedChange={(c) => setSnippet(c === true)}
              aria-label={t('saved.snippet')}
            />
            {t('saved.snippet')}
          </label>
          <IconButton
            label={t('saved.save')}
            icon={<Save size={13} />}
            disabled={!name.trim()}
            onClick={() => void save()}
          />
        </div>
        <div className="flex items-center gap-2">
          <Input
            value={newFolder}
            onChange={(e) => setNewFolder(e.target.value)}
            placeholder={t('saved.newFolder')}
            aria-label={t('saved.newFolder')}
            onKeyDown={(e) => e.key === 'Enter' && void addFolder()}
          />
          <IconButton
            label={t('saved.addFolder')}
            icon={<FolderPlus size={13} />}
            disabled={!newFolder.trim()}
            onClick={() => void addFolder()}
          />
        </div>
      </div>
      <div className="min-h-0 flex-1 overflow-auto">
        {visible.length === 0 && folders.length === 0 && (
          <p className="p-4 text-center text-xs text-muted-foreground">{t('saved.empty')}</p>
        )}
        {groups.map(({ folder, queries }) =>
          folder === null && queries.length === 0 ? null : (
            <section key={folder?.id ?? 'root'} aria-label={folder?.name ?? t('saved.noFolder')}>
              {folder && (
                <h3 className="flex items-center justify-between bg-surface-muted px-2 py-1 text-xs font-medium">
                  {folder.name}
                  <IconButton
                    label={t('saved.deleteFolder', { name: folder.name })}
                    icon={<Trash2 size={12} />}
                    onClick={() => void ipc.savedFolderDelete(folder.id).then(refresh)}
                  />
                </h3>
              )}
              <ul>
                {queries.map((q) => (
                  <li
                    key={q.id}
                    className="group flex items-start gap-1 border-b border-border px-2 py-1.5"
                    data-testid="saved-entry"
                  >
                    <button
                      type="button"
                      className="min-w-0 flex-1 text-left"
                      title={t('saved.insert')}
                      onClick={() => onInsert(q.text, q.isSnippet)}
                    >
                      <span className="flex items-center gap-1 text-xs font-medium">
                        {q.name}
                        {q.isSnippet && <Plus size={10} aria-label={t('saved.snippet')} />}
                      </span>
                      <span className="block truncate font-mono text-[11px] text-muted-foreground">
                        {q.text}
                      </span>
                    </button>
                    <IconButton
                      label={t('saved.delete', { name: q.name })}
                      icon={<Trash2 size={12} />}
                      onClick={() => void ipc.savedQueryDelete(q.id).then(refresh)}
                    />
                  </li>
                ))}
              </ul>
            </section>
          ),
        )}
      </div>
    </div>
  );
}

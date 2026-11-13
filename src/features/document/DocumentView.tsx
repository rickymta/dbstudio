import { useQuery } from '@tanstack/react-query';
import { ChevronLeft, ChevronRight, FileJson, Pencil, Plus, Trash2 } from 'lucide-react';
import { lazy, Suspense, useEffect, useMemo, useState, type FC } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { EmptyState } from '@/components/ui/EmptyState';
import { IconButton } from '@/components/ui/IconButton';
import { Spinner } from '@/components/ui/Spinner';
import { cn } from '@/lib/cn';
import { ipc } from '@/lib/ipc';
import { pathKey } from '@/lib/objectPath';
import type { Editability, IpcError, NotEditableReason, Value } from '@/lib/types';
import { useConnectionProfile } from '@/features/connections/queries';
import { GridStatusBar } from '@/features/data/GridStatusBar';
import { JsonTree } from '@/features/data/JsonTree';
import { ObjectHeader } from '@/features/data/ObjectHeader';
import { valueToPlain } from '@/features/data/format';
import { getGridModel } from '@/features/data/store';
import { useGridVersion } from '@/features/data/useGridVersion';
import { useAppSettings } from '@/features/settings/queries';
import { tableTabId, type TableTabPayload } from '@/stores/tabs';
import { DeleteDocumentDialog, DocumentEditor } from './DocumentEditor';
import { DocumentFilterBar } from './DocumentFilterBar';
import { DocumentTable } from './DocumentTable';
import { toEjson } from './ejson';
import { ColumnTracker } from './flatten';
import { idKey, useDocUi, type DocViewMode } from './store';

const JsonEditor = lazy(() => import('./JsonEditor'));

const MODES: DocViewMode[] = ['table', 'tree', 'json'];

type Dialog = { kind: 'edit'; doc: Value } | { kind: 'insert' } | { kind: 'delete'; doc: Value };

/** Collection tab of a document store: Table (flattened), Tree and JSON views, with editing. */
export const DocumentView: FC<TableTabPayload> = ({ sessionId, connectionId, path }) => {
  const { t } = useTranslation('document');
  const tabId = tableTabId(sessionId, path);
  const model = getGridModel(tabId, sessionId, path);
  const settings = useAppSettings();
  const version = useGridVersion(model);
  const [ui, patchUi] = useDocUi(tabId);
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [tracker] = useState(() => new ColumnTracker());

  useEffect(() => {
    model.setLimits(settings.gridMaxRows, settings.gridMaxCells);
  }, [model, settings.gridMaxRows, settings.gridMaxCells]);
  useEffect(() => {
    model.ensureLoaded();
  }, [model]);

  const flat = useMemo(() => {
    tracker.update(model.rows);
    return { columns: tracker.columns, truncated: tracker.truncated };
  }, [model, version]); // eslint-disable-line react-hooks/exhaustive-deps

  // -- selection ---------------------------------------------------------------------------
  const selectedIndex = useMemo(() => {
    if (ui.selectedId === null) return -1;
    return model.rows.findIndex((r) => idKey(r[0]) === ui.selectedId);
  }, [ui.selectedId, model, version]); // eslint-disable-line react-hooks/exhaustive-deps
  const selectedDoc = selectedIndex >= 0 ? model.rows[selectedIndex]![0]! : undefined;
  const selectRow = (i: number) => patchUi({ selectedId: idKey(model.rows[i]?.[0]) });

  // -- editability -------------------------------------------------------------------------
  const profile = useConnectionProfile(connectionId);
  const editability = useQuery<Editability, IpcError>({
    queryKey: ['documentEditability', sessionId, pathKey(path)],
    queryFn: () => ipc.tableEditability(sessionId, path),
    retry: false,
  });
  let lockedReason: string | null = null;
  if (profile?.readOnly) lockedReason = t('locked.readOnlyConnection');
  else if (editability.isPending) lockedReason = t('locked.loading');
  else if (editability.isError || !editability.data?.editable) {
    const reason: NotEditableReason = editability.data?.reason ?? 'unsupported';
    lockedReason = t(`locked.${reason}`);
  }
  const canEdit = lockedReason === null;
  const canAct = canEdit && selectedDoc !== undefined;

  const empty = model.status === 'ready' && model.rows.length === 0;
  const body = (() => {
    if (model.status === 'error' && model.rows.length === 0 && model.applied.rawWhere === null) {
      return (
        <div role="alert" className="p-4 text-sm break-words text-danger">
          {model.error?.message}
        </div>
      );
    }
    if (model.status === 'loading' && model.rows.length === 0) {
      return (
        <div className="flex flex-1 items-center justify-center">
          <Spinner />
        </div>
      );
    }
    if (empty) {
      return (
        <EmptyState
          icon={<FileJson size={28} />}
          title={model.applied.rawWhere !== null ? t('table.emptyFiltered') : t('table.empty')}
        />
      );
    }
    if (ui.mode === 'table') {
      return (
        <>
          {flat.truncated && (
            <p role="status" className="bg-warning-soft px-3 py-1 text-xs text-warning">
              {t('table.columnsTruncated', { count: tracker.max })}
            </p>
          )}
          <DocumentTable
            model={model}
            columns={flat.columns}
            selectedId={ui.selectedId}
            timestamp={settings.timestampDisplay}
            onSelect={selectRow}
            onOpen={(i) => {
              selectRow(i);
              const d = model.rows[i]?.[0];
              if (canEdit && d) setDialog({ kind: 'edit', doc: d });
            }}
          />
        </>
      );
    }
    return (
      <DocumentDetail
        mode={ui.mode}
        doc={selectedDoc}
        index={selectedIndex}
        total={model.rows.length}
        onMove={(delta) =>
          selectRow(
            Math.min(
              model.rows.length - 1,
              Math.max(0, (selectedIndex < 0 ? 0 : selectedIndex) + delta),
            ),
          )
        }
      />
    );
  })();

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="document-view">
      <ObjectHeader connectionId={connectionId} path={path} />
      <DocumentFilterBar tabId={tabId} model={model} />
      <div className="flex h-8 shrink-0 items-center gap-2 border-b border-border px-2">
        <div
          role="group"
          aria-label={t('view.label')}
          className="flex h-6 overflow-hidden rounded-md border border-border text-xs"
        >
          {MODES.map((m) => (
            <button
              key={m}
              type="button"
              aria-pressed={ui.mode === m}
              onClick={() => patchUi({ mode: m })}
              className={cn(
                'px-2',
                ui.mode === m
                  ? 'bg-accent text-accent-foreground'
                  : 'text-muted-foreground hover:bg-surface-muted',
              )}
            >
              {t(`view.${m}`)}
            </button>
          ))}
        </div>
        <span
          className="min-w-0 flex-1 truncate text-xs text-muted-foreground"
          title={lockedReason ?? undefined}
        >
          {lockedReason}
        </span>
        <Button size="sm" disabled={!canEdit} onClick={() => setDialog({ kind: 'insert' })}>
          <Plus size={12} /> {t('actions.add')}
        </Button>
        <Button
          size="sm"
          disabled={!canAct}
          onClick={() => selectedDoc && setDialog({ kind: 'edit', doc: selectedDoc })}
        >
          <Pencil size={12} /> {t('actions.edit')}
        </Button>
        <Button
          size="sm"
          variant="danger"
          disabled={!canAct}
          onClick={() => selectedDoc && setDialog({ kind: 'delete', doc: selectedDoc })}
        >
          <Trash2 size={12} /> {t('actions.delete')}
        </Button>
      </div>
      <div className="flex min-h-0 flex-1 flex-col">{body}</div>
      <GridStatusBar model={model} />
      {dialog &&
        (dialog.kind === 'delete' ? (
          <DeleteDocumentDialog
            sessionId={sessionId}
            path={path}
            doc={dialog.doc}
            onApplied={() => {
              patchUi({ selectedId: null });
              model.reload();
            }}
            onClose={() => setDialog(null)}
          />
        ) : (
          <DocumentEditor
            sessionId={sessionId}
            path={path}
            doc={dialog.kind === 'edit' ? dialog.doc : null}
            atomic={editability.data?.atomic ?? true}
            onApplied={() => model.reload()}
            onClose={() => setDialog(null)}
          />
        ))}
    </div>
  );
};

/** Tree or JSON rendering of the selected document, with previous/next navigation. */
function DocumentDetail({
  mode,
  doc,
  index,
  total,
  onMove,
}: {
  mode: DocViewMode;
  doc: Value | undefined;
  index: number;
  total: number;
  onMove: (delta: number) => void;
}) {
  const { t } = useTranslation('document');
  const plain = useMemo(() => (doc && mode === 'tree' ? valueToPlain(doc) : null), [doc, mode]);
  const text = useMemo(() => (doc && mode === 'json' ? toEjson(doc) : ''), [doc, mode]);
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex h-7 shrink-0 items-center gap-1 border-b border-border px-2 text-xs text-muted-foreground">
        <IconButton
          label={t('nav.previous')}
          icon={<ChevronLeft size={13} />}
          disabled={total === 0 || index <= 0}
          onClick={() => onMove(-1)}
        />
        <IconButton
          label={t('nav.next')}
          icon={<ChevronRight size={13} />}
          disabled={total === 0 || index >= total - 1}
          onClick={() => onMove(1)}
        />
        {doc && (
          <span className="tabular-nums">{t('nav.position', { index: index + 1, total })}</span>
        )}
      </div>
      {!doc ? (
        <EmptyState icon={<FileJson size={28} />} title={t('nav.noSelection')} />
      ) : mode === 'tree' ? (
        <div className="min-h-0 flex-1 overflow-auto p-2" data-testid="document-tree">
          <JsonTree value={plain} />
        </div>
      ) : (
        <div className="min-h-0 flex-1">
          <Suspense fallback={<Spinner />}>
            <JsonEditor value={text} label={t('view.json')} readOnly />
          </Suspense>
        </div>
      )}
    </div>
  );
}

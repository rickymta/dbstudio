import { ReactFlowProvider } from '@xyflow/react';
import { Network } from 'lucide-react';
import { type FC, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { ConfirmDialog } from '@/components/ui/ConfirmDialog';
import { EmptyState } from '@/components/ui/EmptyState';
import { Spinner } from '@/components/ui/Spinner';
import { toast } from '@/components/ui/toastStore';
import type { IpcError } from '@/lib/types';
import type { ErdTabPayload } from '@/stores/tabs';
import { DdlPanel } from './DdlPanel';
import { ErdCanvas } from './ErdCanvas';
import { ErdToolbar } from './ErdToolbar';
import { LayoutsDialog, SaveLayoutDialog } from './LayoutDialogs';
import { tableLabel } from './model';
import { useErd } from './useErd';

/** Entity-relationship diagram of a database or schema. */
export const ErdTab: FC<ErdTabPayload> = (payload) => (
  <ReactFlowProvider>
    <ErdTabInner {...payload} />
  </ReactFlowProvider>
);

const ErdTabInner: FC<ErdTabPayload> = (payload) => {
  const { t } = useTranslation('erd');
  const erd = useErd(payload);
  const [saveOpen, setSaveOpen] = useState(false);
  const [layoutsOpen, setLayoutsOpen] = useState(false);
  const [exporting, setExporting] = useState(false);

  const scopeName = payload.scope.segments[payload.scope.segments.length - 1]?.name ?? '';

  const submitSearch = () => {
    const first = erd.view.tables.find((tb) => erd.matches.has(tb.key));
    if (first) erd.focusOnTable(first.key);
  };

  const runExport = async (
    format: Parameters<typeof erd.exportAs>[0],
    theme: Parameters<typeof erd.exportAs>[1],
  ) => {
    setExporting(true);
    try {
      const path = await erd.exportAs(format, theme);
      if (path) toast.success(t('export.saved', { path }));
    } catch (e) {
      toast.error(t('export.failed', { message: (e as Partial<IpcError>).message ?? String(e) }));
    } finally {
      setExporting(false);
    }
  };

  let body;
  if (erd.modelError) {
    body = (
      <EmptyState
        className="h-full"
        icon={<Network size={32} />}
        title={erd.modelError.kind === 'unsupported' ? t('error.unsupported') : t('error.title')}
        description={erd.modelError.kind === 'unsupported' ? undefined : erd.modelError.message}
        action={
          erd.modelError.kind === 'unsupported' ? undefined : (
            <Button size="sm" onClick={erd.refresh}>
              {t('error.retry')}
            </Button>
          )
        }
      />
    );
  } else if (erd.modelLoading) {
    body = (
      <div className="flex h-full items-center justify-center gap-2 text-muted-foreground">
        <Spinner /> {t('loading')}
      </div>
    );
  } else if (erd.model && erd.view.tables.length === 0) {
    body = (
      <EmptyState
        className="h-full"
        icon={<Network size={32} />}
        title={t('empty.title')}
        description={t('empty.description')}
      />
    );
  } else if (!erd.ready) {
    body = (
      <div className="flex h-full items-center justify-center gap-2 text-muted-foreground">
        <Spinner /> {t('laying')}
      </div>
    );
  } else {
    body = (
      <ErdCanvas
        nodes={erd.nodes}
        edges={erd.edges}
        viewport={erd.viewport}
        onMove={erd.move}
        onSelect={erd.setSelectedKey}
      />
    );
  }

  const missingCount = erd.diff.missing.length;
  const changedCount = erd.diff.changed.length;
  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="erd-tab">
      <ErdToolbar
        options={erd.options}
        onOptions={erd.setOptions}
        search={erd.search}
        onSearch={erd.setSearch}
        matchCount={erd.matches.size}
        onSubmitSearch={submitSearch}
        selectedKey={erd.selectedKey}
        focus={erd.focus}
        onFocus={erd.setFocus}
        shown={erd.visibleCount}
        total={erd.view.tables.length}
        busy={erd.layingOut}
        showDdl={erd.showDdl && erd.selectedTable !== undefined}
        onToggleDdl={() => erd.setShowDdl(!erd.showDdl)}
        canColor={erd.selectedTable !== undefined}
        onColor={(c) => erd.selectedKey && erd.setColor(erd.selectedKey, c)}
        onRelayout={() => void erd.relayout()}
        onFit={erd.fit}
        onRefresh={erd.refresh}
        onSave={() => setSaveOpen(true)}
        onOpenLayouts={() => setLayoutsOpen(true)}
        layoutName={erd.layoutName}
        onExport={(f, th) => void runExport(f, th)}
        exporting={exporting}
      />
      {(missingCount > 0 || changedCount > 0) && (
        <div
          role="status"
          className="flex shrink-0 items-center gap-3 border-b border-border bg-warning-soft px-3 py-1 text-xs"
          data-testid="erd-diff-banner"
        >
          {missingCount > 0 && (
            <span className="text-danger">{t('diff.missing', { n: missingCount })}</span>
          )}
          {changedCount > 0 && <span>{t('diff.changed', { n: changedCount })}</span>}
          <span className="text-muted-foreground">{t('diff.hint')}</span>
          {missingCount > 0 && (
            <Button size="sm" onClick={erd.forgetMissing}>
              {t('diff.forget')}
            </Button>
          )}
        </div>
      )}
      {erd.truncated && (
        <div
          role="status"
          className="shrink-0 border-b border-border bg-warning-soft px-3 py-1 text-xs"
          data-testid="erd-truncated-banner"
        >
          {t('truncated')}
        </div>
      )}
      <div className="flex min-h-0 flex-1">
        <div className="min-w-0 flex-1" data-testid="erd-canvas">
          {body}
        </div>
        {erd.showDdl && erd.selectedTable && (
          <DdlPanel
            sessionId={payload.sessionId}
            connectionId={payload.connectionId}
            path={erd.selectedTable.table.path}
            title={tableLabel(erd.selectedTable.table)}
            onClose={() => erd.setShowDdl(false)}
          />
        )}
      </div>
      {saveOpen && (
        <SaveLayoutDialog
          open
          onOpenChange={setSaveOpen}
          initialName={erd.layoutName ?? t('save.defaultName', { name: scopeName })}
          canSaveAsNew={erd.activeLayoutId !== null}
          onSave={erd.save}
        />
      )}
      <ConfirmDialog
        open={erd.conflict !== null}
        onOpenChange={(open) => {
          if (!open) void erd.resolveConflict('reload');
        }}
        title={t('conflict.title')}
        description={t('conflict.description')}
        confirmLabel={t('conflict.overwrite')}
        cancelLabel={t('conflict.reload')}
        destructive
        onConfirm={() => void erd.resolveConflict('overwrite')}
      />
      <LayoutsDialog
        open={layoutsOpen}
        onOpenChange={setLayoutsOpen}
        connectionId={payload.connectionId}
        scope={payload.scope}
        activeId={erd.activeLayoutId}
        onOpenLayout={erd.openLayout}
        onDeleteLayout={erd.deleteLayout}
      />
    </div>
  );
};

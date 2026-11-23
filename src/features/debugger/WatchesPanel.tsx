import { Minus, Plus } from 'lucide-react';
import { useRef, useState, type FC, type KeyboardEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Input } from '@/components/ui/Input';
import { Tooltip } from '@/components/ui/Tooltip';
import { cn } from '@/lib/cn';
import type { WatchResult } from '@/lib/types';
import { addWatch, editWatch, moveWatch, removeWatch } from './debugActions';
import { useDebugStore } from './debugStore';
import { MiniGrid } from './MiniGrid';
import { evalTypeName, evalValueText } from './valueText';

const EMPTY: never[] = [];

const WatchValue: FC<{ result: WatchResult | undefined }> = ({ result }) => {
  const { t } = useTranslation('debugger');
  if (!result) return <span className="text-muted-foreground">{t('watches.pending')}</span>;
  const v = result.result.value;
  if (v.kind === 'error') return <span className="text-danger">{v.error.message}</span>;
  if (v.kind === 'skipped')
    return (
      <span className="text-muted-foreground" title={v.reason}>
        {t('watches.skipped', { reason: v.reason })}
      </span>
    );
  const type = evalTypeName(result.result);
  return (
    <>
      <span className="font-mono">{evalValueText(v)}</span>
      {type && <span className="ml-1 text-xs text-muted-foreground">{`{${type}}`}</span>}
    </>
  );
};

/** Watch expressions: add (+), remove (-), edit in place (F2 / double click), drag to reorder. */
export const WatchesPanel: FC<{ tabId: string }> = ({ tabId }) => {
  const { t } = useTranslation('debugger');
  const specs = useDebugStore((s) => s.watches[tabId] ?? EMPTY);
  const results = useDebugStore((s) => s.sessions[tabId]?.state.watches ?? EMPTY);
  const [selected, setSelected] = useState<string | null>(null);
  const [editing, setEditing] = useState<string | 'new' | null>(null);
  const [dragging, setDragging] = useState<string | null>(null);

  // Enter unmounts the input, which may blur it too: only the first end counts.
  const editRef = useRef<string | null>(null);
  const begin = (id: string) => {
    editRef.current = id;
    setEditing(id);
  };
  const cancel = () => {
    editRef.current = null;
    setEditing(null);
  };
  const finish = (id: string | 'new', text: string) => {
    if (editRef.current !== id) return;
    cancel();
    if (id === 'new') void addWatch(tabId, text);
    else void editWatch(tabId, id, text);
  };

  const onRowKey = (e: KeyboardEvent, id: string) => {
    if (e.key === 'F2') {
      e.preventDefault();
      begin(id);
    } else if (e.key === 'Delete' || e.key === 'Backspace') {
      e.preventDefault();
      void removeWatch(tabId, id);
    } else if (e.key === 'Insert') {
      e.preventDefault();
      begin('new');
    }
  };

  return (
    <section className="flex h-full min-h-0 flex-col" aria-label={t('watches.title')}>
      <header className="flex shrink-0 items-center border-b border-border px-2 py-0.5">
        <span className="flex-1 text-[11px] font-medium text-muted-foreground uppercase">
          {t('watches.title')}
        </span>
        <Tooltip content={t('watches.add')}>
          <Button
            size="icon"
            variant="ghost"
            className="size-6 min-w-6"
            aria-label={t('watches.add')}
            onClick={() => begin('new')}
          >
            <Plus size={13} aria-hidden />
          </Button>
        </Tooltip>
        <Tooltip content={t('watches.remove')}>
          <Button
            size="icon"
            variant="ghost"
            className="size-6 min-w-6"
            aria-label={t('watches.remove')}
            disabled={!selected}
            onClick={() => selected && void removeWatch(tabId, selected)}
          >
            <Minus size={13} aria-hidden />
          </Button>
        </Tooltip>
      </header>
      <div
        role="tree"
        aria-label={t('watches.title')}
        className="min-h-0 flex-1 overflow-auto py-0.5"
      >
        {specs.length === 0 && editing !== 'new' && (
          <p className="px-2 py-1 text-xs text-muted-foreground">{t('watches.empty')}</p>
        )}
        {specs.map((w) => {
          const result = results.find((r) => r.id === w.id);
          const rows = result?.result.value.kind === 'rows' ? result.result.value.result : null;
          return (
            <div
              key={w.id}
              role="treeitem"
              aria-level={1}
              aria-selected={selected === w.id}
              tabIndex={0}
              draggable={editing !== w.id}
              data-watch={w.id}
              className={cn(
                'px-2 py-0.5 text-[13px] outline-none focus-visible:ring-1 focus-visible:ring-ring',
                selected === w.id ? 'bg-accent/15' : 'hover:bg-surface-muted',
                dragging === w.id && 'opacity-50',
              )}
              onClick={() => setSelected(w.id)}
              onDoubleClick={() => begin(w.id)}
              onKeyDown={(e) => onRowKey(e, w.id)}
              onDragStart={() => setDragging(w.id)}
              onDragEnd={() => setDragging(null)}
              onDragOver={(e) => dragging && e.preventDefault()}
              onDrop={() => {
                if (dragging) void moveWatch(tabId, dragging, w.id);
                setDragging(null);
              }}
            >
              {editing === w.id ? (
                <Input
                  autoFocus
                  className="h-6 text-xs"
                  defaultValue={w.expr}
                  aria-label={t('watches.edit')}
                  onBlur={(e) => finish(w.id, e.currentTarget.value)}
                  onKeyDown={(e) => {
                    e.stopPropagation();
                    if (e.key === 'Enter') finish(w.id, e.currentTarget.value);
                    else if (e.key === 'Escape') cancel();
                  }}
                />
              ) : (
                <div className="flex min-w-0 items-baseline gap-1">
                  <span className="shrink-0 font-mono">{w.expr}</span>
                  <span className="text-muted-foreground">=</span>
                  <span className="min-w-0 truncate">
                    <WatchValue result={result} />
                  </span>
                </div>
              )}
              {rows && editing !== w.id && (
                <MiniGrid columns={rows.columns} rows={rows.rows} truncated={rows.truncated} />
              )}
            </div>
          );
        })}
        {editing === 'new' && (
          <div className="px-2 py-0.5">
            <Input
              autoFocus
              className="h-6 text-xs"
              aria-label={t('watches.new')}
              placeholder={t('watches.new')}
              onBlur={(e) => finish('new', e.currentTarget.value)}
              onKeyDown={(e) => {
                e.stopPropagation();
                if (e.key === 'Enter') finish('new', e.currentTarget.value);
                else if (e.key === 'Escape') cancel();
              }}
            />
          </div>
        )}
      </div>
    </section>
  );
};

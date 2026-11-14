import { ChevronDown, ChevronRight } from 'lucide-react';
import { useEffect, useMemo, useRef, useState, type FC, type KeyboardEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Input } from '@/components/ui/Input';
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from '@/components/ui/ContextMenu';
import { toast } from '@/components/ui/toastStore';
import { valueToText } from '@/features/data/format';
import { cn } from '@/lib/cn';
import type { DebugVariable } from '@/lib/types';
import { addWatch, loadChildren, setVariable } from './debugActions';
import { useDebugStore } from './debugStore';
import { useDebugUi } from './debugUiStore';

const NO_OVERRIDES: Record<string, DebugVariable> = {};

interface Row {
  variable: DebugVariable;
  depth: number;
}

const fullText = (v: DebugVariable): string => (v.value ? valueToText(v.value) : v.display);

/** Variables tree: lazy groups, `{type}` in grey, changed values highlighted, Set Value. */
export const VariablesPanel: FC<{ tabId: string }> = ({ tabId }) => {
  const { t } = useTranslation('debugger');
  const state = useDebugStore((s) => s.sessions[tabId]?.state);
  const children = useDebugStore((s) => s.children[tabId]);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<string | null>(null);
  const [editing, setEditing] = useState<string | null>(null);
  const [overrideState, setOverrideState] = useState<{
    seq: number;
    map: Record<string, DebugVariable>;
  }>({ seq: 0, map: {} });
  const [prompt, setPrompt] = useState('');
  const seq = state?.seq ?? 0;
  // Edited values are shown until the next snapshot replaces them.
  const overrides = overrideState.seq === seq ? overrideState.map : NO_OVERRIDES;
  const expandedRef = useRef(expanded);
  useEffect(() => {
    expandedRef.current = expanded;
  });

  // A new snapshot invalidates the loaded children: reload the ones the user has open.
  useEffect(() => {
    const open = [...expandedRef.current];
    for (const reference of open) {
      loadChildren(tabId, reference).catch(() => {
        setExpanded((prev) => {
          const next = new Set(prev);
          next.delete(reference);
          return next;
        });
      });
    }
  }, [tabId, seq]);

  const rows = useMemo(() => {
    const out: Row[] = [];
    const walk = (list: DebugVariable[], depth: number) => {
      for (const raw of list) {
        const variable = overrides[raw.reference] ?? raw;
        out.push({ variable, depth });
        if (variable.hasChildren && expanded.has(variable.reference)) {
          walk(children?.[variable.reference] ?? [], depth + 1);
        }
      }
    };
    walk(state?.variables ?? [], 0);
    return out;
  }, [state?.variables, children, expanded, overrides]);

  const toggle = (v: DebugVariable) => {
    if (!v.hasChildren) return;
    if (expanded.has(v.reference)) {
      setExpanded((prev) => {
        const next = new Set(prev);
        next.delete(v.reference);
        return next;
      });
      return;
    }
    setExpanded((prev) => new Set(prev).add(v.reference));
    if (!children?.[v.reference]) void loadChildren(tabId, v.reference).catch(() => undefined);
  };

  const commit = async (v: DebugVariable, text: string) => {
    setEditing(null);
    const updated = await setVariable(tabId, v.reference, text);
    if (updated)
      setOverrideState((prev) => ({
        seq: seq,
        map: { ...(prev.seq === seq ? prev.map : {}), [updated.reference]: updated },
      }));
  };

  const copy = (text: string) => {
    void navigator.clipboard?.writeText(text).catch(() => toast.error(t('grid.copyFailed')));
  };

  const onKey = (e: KeyboardEvent, index: number) => {
    const row = rows[index];
    if (!row) return;
    const move = (to: number) => {
      const target = rows[to];
      if (!target) return;
      e.preventDefault();
      setSelected(target.variable.reference);
      (e.currentTarget.parentElement?.children[to] as HTMLElement | undefined)?.focus();
    };
    if (e.key === 'ArrowDown') move(index + 1);
    else if (e.key === 'ArrowUp') move(index - 1);
    else if (
      e.key === 'ArrowRight' &&
      row.variable.hasChildren &&
      !expanded.has(row.variable.reference)
    ) {
      e.preventDefault();
      toggle(row.variable);
    } else if (e.key === 'ArrowLeft' && expanded.has(row.variable.reference)) {
      e.preventDefault();
      toggle(row.variable);
    } else if (e.key === 'F2' && row.variable.editable) {
      e.preventDefault();
      setEditing(row.variable.reference);
    }
  };

  const onPrompt = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key !== 'Enter' || !prompt.trim()) return;
    e.preventDefault();
    if (e.shiftKey) void addWatch(tabId, prompt);
    else useDebugUi.getState().openDialog(tabId, { kind: 'evaluate', expr: prompt });
    setPrompt('');
  };

  return (
    <section className="flex h-full min-h-0 flex-col" aria-label={t('variables.title')}>
      <header className="shrink-0 border-b border-border px-2 py-1 text-[11px] font-medium text-muted-foreground uppercase">
        {t('variables.title')}
      </header>
      <div className="shrink-0 border-b border-border p-1">
        <Input
          className="h-7 text-xs"
          value={prompt}
          placeholder={t('variables.prompt')}
          aria-label={t('variables.prompt')}
          onChange={(e) => setPrompt(e.target.value)}
          onKeyDown={onPrompt}
        />
      </div>
      <div
        role="treegrid"
        aria-label={t('variables.title')}
        className="min-h-0 flex-1 overflow-auto py-0.5"
      >
        {rows.length === 0 && (
          <p className="px-2 py-1 text-xs text-muted-foreground">{t('variables.empty')}</p>
        )}
        {rows.map(({ variable: v, depth }, index) => {
          const open = expanded.has(v.reference);
          const isEditing = editing === v.reference;
          const row = (
            <div
              role="row"
              aria-level={depth + 1}
              aria-expanded={v.hasChildren ? open : undefined}
              aria-selected={selected === v.reference}
              tabIndex={selected === v.reference || (selected === null && index === 0) ? 0 : -1}
              data-reference={v.reference}
              data-changed={v.changed || undefined}
              className={cn(
                'flex items-center gap-1 px-1 py-px text-[13px] outline-none',
                'focus-visible:ring-1 focus-visible:ring-ring',
                selected === v.reference ? 'bg-accent/15' : 'hover:bg-surface-muted',
                v.kind === 'group' && 'font-medium',
              )}
              style={{ paddingLeft: depth * 14 + 4 }}
              onClick={() => setSelected(v.reference)}
              onDoubleClick={() => (v.editable ? setEditing(v.reference) : toggle(v))}
              onKeyDown={(e) => onKey(e, index)}
            >
              <button
                type="button"
                tabIndex={-1}
                aria-label={open ? t('variables.collapse') : t('variables.expand')}
                className={cn(
                  'size-4 shrink-0 text-muted-foreground',
                  !v.hasChildren && 'invisible',
                )}
                onClick={(e) => {
                  e.stopPropagation();
                  toggle(v);
                }}
              >
                {open ? (
                  <ChevronDown size={13} aria-hidden />
                ) : (
                  <ChevronRight size={13} aria-hidden />
                )}
              </button>
              <span role="gridcell" className="shrink-0">
                {v.name}
              </span>
              {v.kind !== 'group' || v.display ? (
                <span className="text-muted-foreground">=</span>
              ) : null}
              <span
                role="gridcell"
                className={cn(
                  'min-w-0 flex-1 truncate font-mono',
                  v.changed && 'rounded bg-warning-soft px-1 text-warning',
                )}
                title={fullText(v)}
              >
                {isEditing ? (
                  <Input
                    autoFocus
                    className="h-6 text-xs"
                    defaultValue={fullText(v)}
                    aria-label={t('variables.setValue', { name: v.name })}
                    onClick={(e) => e.stopPropagation()}
                    onKeyDown={(e) => {
                      e.stopPropagation();
                      if (e.key === 'Enter') void commit(v, e.currentTarget.value);
                      else if (e.key === 'Escape') setEditing(null);
                    }}
                  />
                ) : (
                  v.display
                )}
              </span>
              {v.typeName && (
                <span role="gridcell" className="shrink-0 text-xs text-muted-foreground">
                  {`{${v.typeName}}`}
                </span>
              )}
            </div>
          );
          return (
            <ContextMenu key={v.reference}>
              <ContextMenuTrigger asChild>{row}</ContextMenuTrigger>
              <ContextMenuContent>
                <ContextMenuItem disabled={!v.editable} onSelect={() => setEditing(v.reference)}>
                  {t('variables.menu.setValue')}
                </ContextMenuItem>
                <ContextMenuItem onSelect={() => copy(fullText(v))}>
                  {t('variables.menu.copyValue')}
                </ContextMenuItem>
                <ContextMenuItem onSelect={() => copy(v.name)}>
                  {t('variables.menu.copyName')}
                </ContextMenuItem>
                <ContextMenuSeparator />
                <ContextMenuItem
                  disabled={v.kind === 'group'}
                  onSelect={() => void addWatch(tabId, v.name)}
                >
                  {t('variables.menu.addWatch')}
                </ContextMenuItem>
                <ContextMenuItem
                  onSelect={() =>
                    useDebugUi
                      .getState()
                      .openDialog(tabId, { kind: 'inspect', name: v.name, text: fullText(v) })
                  }
                >
                  {t('variables.menu.inspect')}
                </ContextMenuItem>
              </ContextMenuContent>
            </ContextMenu>
          );
        })}
      </div>
    </section>
  );
};

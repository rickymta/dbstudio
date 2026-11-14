import { GridActions } from './GridActions';
import { ChevronDown, ChevronUp, Filter, Plus, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Checkbox } from '@/components/ui/Checkbox';
import { IconButton } from '@/components/ui/IconButton';
import { Input } from '@/components/ui/Input';
import { Select } from '@/components/ui/Select';
import { cn } from '@/lib/cn';
import { FILTER_OPS, type FilterOp } from '@/lib/types';
import type { GridModel } from './gridModel';
import {
  buildApplied,
  isFilterActive,
  opIsUnary,
  type FilterDraft,
  type FilterRowDraft,
} from './request';
import { useTabUi } from './store';
import { useGridVersion } from './useGridVersion';

let rowSeq = 0;
const newRowId = () => `f${++rowSeq}`;

interface FilterBarProps {
  tabId: string;
  model: GridModel;
}

/** Filter conditions or a raw WHERE; Apply (Enter) sends them to the server. */
export function FilterBar({ tabId, model }: FilterBarProps) {
  const { t } = useTranslation('data');
  useGridVersion(model);
  const [ui, patchUi] = useTabUi(tabId);
  const draft = ui.draft;
  const setDraft = (next: FilterDraft) => patchUi({ draft: next });

  const columnOptions = model.columns.map((c) => ({ value: c.name, label: c.name }));
  const opOptions = FILTER_OPS.map((op) => ({ value: op, label: t(`filterOp.${op}`) }));
  const dirty = JSON.stringify(buildApplied(draft)) !== JSON.stringify(model.applied);
  const active = isFilterActive(model.applied);

  const apply = () => model.applyFilter(draft);
  const clear = () => {
    setDraft({ mode: draft.mode, rows: [], raw: '' });
    model.clearFilter();
  };
  const addRow = () =>
    setDraft({
      ...draft,
      rows: [
        ...draft.rows,
        {
          id: newRowId(),
          enabled: true,
          column: model.columns[0]?.name ?? '',
          op: 'eq',
          value: '',
        },
      ],
    });
  const updateRow = (id: string, patch: Partial<FilterRowDraft>) =>
    setDraft({
      ...draft,
      rows: draft.rows.map((r) => (r.id === id ? { ...r, ...patch } : r)),
    });
  const removeRow = (id: string) =>
    setDraft({ ...draft, rows: draft.rows.filter((r) => r.id !== id) });

  const onEnter = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      apply();
    }
  };

  const filterError = active && model.status === 'error' ? model.error : null;

  return (
    <div className="shrink-0 border-b border-border bg-surface" role="search">
      <div className="flex min-h-10 flex-wrap items-center gap-2 px-3 py-1">
        <Filter
          size={13}
          className={cn('shrink-0', active ? 'text-accent' : 'text-muted-foreground')}
        />
        <div className="flex min-h-7 overflow-hidden rounded-md border border-border text-xs">
          {(['builder', 'raw'] as const).map((mode) => (
            <button
              key={mode}
              type="button"
              aria-pressed={draft.mode === mode}
              onClick={() => setDraft({ ...draft, mode })}
              className={cn(
                'px-2',
                draft.mode === mode
                  ? 'bg-accent text-accent-foreground'
                  : 'text-muted-foreground hover:bg-surface-muted',
              )}
            >
              {mode === 'builder' ? t('filter.builderMode') : t('filter.rawMode')}
            </button>
          ))}
        </div>
        {draft.mode === 'builder' && !ui.filterCollapsed && (
          <Button size="sm" variant="ghost" onClick={addRow}>
            <Plus size={12} /> {t('filter.add')}
          </Button>
        )}
        {draft.mode === 'raw' && !ui.filterCollapsed && (
          <Input
            aria-label={t('filter.rawLabel')}
            placeholder={t('filter.rawPlaceholder')}
            value={draft.raw}
            onChange={(e) => setDraft({ ...draft, raw: e.target.value })}
            onKeyDown={onEnter}
            className="min-w-40 flex-1 font-mono text-xs"
          />
        )}
        {dirty && (
          <span role="status" className="text-xs text-warning">
            {t('filter.pending')}
          </span>
        )}
        {ui.filterCollapsed && active && (
          <span
            className="max-w-72 truncate text-xs text-muted-foreground"
            title={
              model.applied.rawWhere ??
              model.applied.filters
                .map((f) => `${f.column} ${t(`filterOp.${f.op}`)} ${f.values.join(', ')}`)
                .join(' AND ')
            }
          >
            {model.applied.rawWhere ??
              model.applied.filters
                .map((f) => `${f.column} ${t(`filterOp.${f.op}`)} ${f.values.join(', ')}`)
                .join(' AND ')}
          </span>
        )}
        {active && (
          <span className="text-xs text-accent">
            {model.applied.rawWhere !== null
              ? t('filter.rawActive')
              : t('filter.active', { count: model.applied.filters.length })}
          </span>
        )}
        <Button size="sm" variant="primary" onClick={apply} disabled={model.status === 'loading'}>
          {t('filter.apply')}
        </Button>
        <Button
          size="sm"
          variant="outline"
          onClick={clear}
          disabled={!active && isDraftEmpty(draft)}
        >
          {t('filter.clear')}
        </Button>
        <IconButton
          label={t(ui.filterCollapsed ? 'filter.expand' : 'filter.collapse')}
          icon={ui.filterCollapsed ? <ChevronDown size={14} /> : <ChevronUp size={14} />}
          aria-expanded={!ui.filterCollapsed}
          onClick={() => patchUi({ filterCollapsed: !ui.filterCollapsed })}
        />
        <div className="ml-auto">
          <GridActions model={model} tabId={tabId} />
        </div>
      </div>

      {draft.mode === 'builder' &&
        !ui.filterCollapsed &&
        draft.rows.map((r) => (
          <div key={r.id} className="flex min-h-10 flex-wrap items-center gap-2 px-3 py-1 pl-7">
            <Checkbox
              aria-label={t('filter.enabled')}
              checked={r.enabled}
              onCheckedChange={(c) => updateRow(r.id, { enabled: c === true })}
            />
            <div className="w-44 max-w-full">
              <Select
                aria-label={t('filter.column')}
                value={r.column || undefined}
                placeholder={t('filter.noColumns')}
                options={columnOptions}
                onValueChange={(column) => updateRow(r.id, { column })}
                className="text-xs"
              />
            </div>
            <div className="w-40 max-w-full">
              <Select
                aria-label={t('filter.operator')}
                value={r.op}
                options={opOptions}
                onValueChange={(op) => updateRow(r.id, { op: op as FilterOp })}
                className="text-xs"
              />
            </div>
            {!opIsUnary(r.op) && (
              <Input
                aria-label={t('filter.value')}
                placeholder={r.op === 'in' ? t('filter.valueIn') : t('filter.valuePlaceholder')}
                value={r.value}
                onChange={(e) => updateRow(r.id, { value: e.target.value })}
                onKeyDown={onEnter}
                className="min-w-32 max-w-sm flex-1 font-mono text-xs"
              />
            )}
            <IconButton
              label={t('filter.remove')}
              icon={<X size={13} />}
              onClick={() => removeRow(r.id)}
            />
          </div>
        ))}

      {filterError && (
        <div
          role="alert"
          className="border-t border-danger/30 bg-danger-soft px-3 py-1 font-mono text-xs break-words text-danger"
        >
          <span className="font-sans font-medium">{t('filter.errorTitle')}: </span>
          {filterError.message}
        </div>
      )}
    </div>
  );
}

function isDraftEmpty(d: FilterDraft): boolean {
  return d.rows.length === 0 && d.raw === '';
}

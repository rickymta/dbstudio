import { Filter, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Input } from '@/components/ui/Input';
import { cn } from '@/lib/cn';
import type { GridModel } from '@/features/data/gridModel';
import { useTabUi } from '@/features/data/store';
import { useGridVersion } from '@/features/data/useGridVersion';

/**
 * Filter of a collection: one EJSON (or shell literal) object sent as `rawWhere`, plus the active
 * sort keys. Sorting itself is changed from the column headers of the table.
 */
export function DocumentFilterBar({ tabId, model }: { tabId: string; model: GridModel }) {
  const { t } = useTranslation('document');
  useGridVersion(model);
  const [ui, patchUi] = useTabUi(tabId);
  const raw = ui.draft.raw;
  const active = model.applied.rawWhere !== null;
  const filterError = active && model.status === 'error' ? model.error : null;

  const apply = () => model.applyFilter({ ...ui.draft, mode: 'raw' });
  const clear = () => {
    patchUi({ draft: { ...ui.draft, mode: 'raw', raw: '' } });
    model.clearFilter();
  };

  return (
    <div className="shrink-0 border-b border-border bg-surface" role="search">
      <div className="flex h-8 items-center gap-1.5 px-2">
        <Filter
          size={13}
          className={cn('shrink-0', active ? 'text-accent' : 'text-muted-foreground')}
        />
        <Input
          aria-label={t('filter.label')}
          placeholder={t('filter.placeholder')}
          value={raw}
          onChange={(e) => patchUi({ draft: { ...ui.draft, mode: 'raw', raw: e.target.value } })}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              e.preventDefault();
              apply();
            }
          }}
          className="h-6 flex-1 font-mono text-xs"
        />
        {active && <span className="text-xs text-accent">{t('filter.active')}</span>}
        {model.sort.length > 0 && (
          <span className="flex items-center gap-1 text-xs text-muted-foreground">
            {t('filter.sort')}:{' '}
            {model.sort.map((s) => `${s.column} ${s.descending ? '↓' : '↑'}`).join(', ')}
            <button
              type="button"
              aria-label={t('filter.clearSort')}
              title={t('filter.clearSort')}
              onClick={() => model.setSort([])}
              className="rounded p-0.5 hover:bg-surface-muted"
            >
              <X size={11} />
            </button>
          </span>
        )}
        <Button size="sm" variant="primary" onClick={apply}>
          {t('filter.apply')}
        </Button>
        <Button size="sm" variant="outline" onClick={clear} disabled={!active && raw === ''}>
          {t('filter.clear')}
        </Button>
      </div>
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

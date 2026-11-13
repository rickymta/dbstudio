import { useQuery } from '@tanstack/react-query';
import { Key } from 'lucide-react';
import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { EmptyState } from '@/components/ui/EmptyState';
import { Spinner } from '@/components/ui/Spinner';
import { ipc } from '@/lib/ipc';
import { lastSegment, pathKey } from '@/lib/objectPath';
import type { IpcError, ObjectPath, TableDetails, Uuid } from '@/lib/types';
import { formatBytes } from './format';

const tableDetailsKey = (sessionId: Uuid, path: ObjectPath) =>
  ['tableDetails', sessionId, pathKey(path)] as const;

const th = 'sticky top-0 h-6 border-b border-border bg-surface-muted px-2 text-left font-medium';
const td = 'h-6 border-b border-border/60 px-2 align-middle';

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="mb-4">
      <h3 className="mb-1 px-3 text-xs font-semibold tracking-wide text-muted-foreground uppercase">
        {title}
      </h3>
      <div className="overflow-x-auto">{children}</div>
    </section>
  );
}

function Mono({ children }: { children: ReactNode }) {
  return <span className="font-mono">{children}</span>;
}

function Dash() {
  return <span className="text-muted-foreground">-</span>;
}

/** Columns, indexes and foreign keys of a table/view (BROWSE-08). */
export function StructureTab({ sessionId, path }: { sessionId: Uuid; path: ObjectPath }) {
  const { t, i18n } = useTranslation('data');
  const query = useQuery<TableDetails, IpcError>({
    queryKey: tableDetailsKey(sessionId, path),
    queryFn: () => ipc.tableDetails(sessionId, path),
    retry: false,
  });

  if (query.isPending) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-muted-foreground">
        <Spinner /> {t('structure.loading')}
      </div>
    );
  }
  if (query.isError) {
    return (
      <EmptyState
        className="h-full"
        title={t('structure.errorTitle')}
        description={query.error.message}
        action={
          <Button size="sm" onClick={() => void query.refetch()}>
            {t('structure.retry')}
          </Button>
        }
      />
    );
  }

  const d = query.data;
  const nf = new Intl.NumberFormat(i18n.language);
  const yesNo = (b: boolean) => (b ? t('structure.yes') : t('structure.no'));
  const kindLabel =
    d.kind === 'view'
      ? t('structure.kindView')
      : d.kind === 'materializedView'
        ? t('structure.kindMaterializedView')
        : t('structure.kindTable');
  const unknown = t('structure.summaryUnknown');
  const refName = (p: ObjectPath, cols: string[]) =>
    `${lastSegment(p)?.name ?? ''}(${cols.join(', ')})`;

  return (
    <div className="h-full overflow-auto py-2 text-xs select-text">
      <div
        className="mb-3 flex flex-wrap items-center gap-x-5 gap-y-1 px-3"
        data-testid="structure-summary"
      >
        <span className="font-medium">{kindLabel}</span>
        <span>
          <span className="text-muted-foreground">{t('structure.summaryRows')}: </span>
          {d.estimatedRows != null ? `~${nf.format(d.estimatedRows)}` : unknown}
        </span>
        <span>
          <span className="text-muted-foreground">{t('structure.summarySize')}: </span>
          {d.sizeBytes != null ? formatBytes(d.sizeBytes) : unknown}
        </span>
        {d.comment && <span className="text-muted-foreground">{d.comment}</span>}
      </div>

      <Section title={t('structure.columns')}>
        <table className="w-full border-collapse" aria-label={t('structure.columns')}>
          <thead>
            <tr>
              <th className={th}>{t('structure.ordinal')}</th>
              <th className={th}>{t('structure.name')}</th>
              <th className={th}>{t('structure.type')}</th>
              <th className={th}>{t('structure.nullable')}</th>
              <th className={th}>{t('structure.default')}</th>
              <th className={th}>{t('structure.primaryKey')}</th>
              <th className={th}>{t('structure.extra')}</th>
              <th className={th}>{t('structure.comment')}</th>
            </tr>
          </thead>
          <tbody>
            {d.columns.map((c) => (
              <tr key={c.name} className="hover:bg-surface-muted">
                <td className={`${td} text-muted-foreground tabular-nums`}>{c.ordinal}</td>
                <td className={`${td} font-medium`}>{c.name}</td>
                <td className={td}>
                  <Mono>{c.typeName}</Mono>
                </td>
                <td className={td}>{yesNo(c.nullable)}</td>
                <td className={td}>{c.default ? <Mono>{c.default}</Mono> : <Dash />}</td>
                <td className={td}>
                  {c.primaryKey ? (
                    <Key
                      size={12}
                      aria-label={t('structure.primaryKey')}
                      className="text-warning"
                    />
                  ) : (
                    <Dash />
                  )}
                </td>
                <td className={td}>{c.extra ? <Mono>{c.extra}</Mono> : <Dash />}</td>
                <td className={td}>{c.comment ?? <Dash />}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </Section>

      {d.indexes.length > 0 && (
        <Section title={t('structure.indexes')}>
          <table className="w-full border-collapse" aria-label={t('structure.indexes')}>
            <thead>
              <tr>
                <th className={th}>{t('structure.name')}</th>
                <th className={th}>{t('structure.columnsCol')}</th>
                <th className={th}>{t('structure.unique')}</th>
                <th className={th}>{t('structure.primary')}</th>
                <th className={th}>{t('structure.method')}</th>
              </tr>
            </thead>
            <tbody>
              {d.indexes.map((i) => (
                <tr key={i.name} className="hover:bg-surface-muted">
                  <td className={`${td} font-medium`}>{i.name}</td>
                  <td className={td}>
                    <Mono>{i.columns.join(', ')}</Mono>
                  </td>
                  <td className={td}>{yesNo(i.unique)}</td>
                  <td className={td}>{yesNo(i.primary)}</td>
                  <td className={td}>{i.method ?? <Dash />}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </Section>
      )}

      {d.foreignKeys.length > 0 && (
        <Section title={t('structure.foreignKeys')}>
          <table className="w-full border-collapse" aria-label={t('structure.foreignKeys')}>
            <thead>
              <tr>
                <th className={th}>{t('structure.name')}</th>
                <th className={th}>{t('structure.columnsCol')}</th>
                <th className={th}>{t('structure.references')}</th>
                <th className={th}>{t('structure.onUpdate')}</th>
                <th className={th}>{t('structure.onDelete')}</th>
              </tr>
            </thead>
            <tbody>
              {d.foreignKeys.map((f) => (
                <tr key={f.name} className="hover:bg-surface-muted">
                  <td className={`${td} font-medium`}>{f.name}</td>
                  <td className={td}>
                    <Mono>{f.columns.join(', ')}</Mono>
                  </td>
                  <td className={td}>
                    <Mono>{refName(f.referencedTable, f.referencedColumns)}</Mono>
                  </td>
                  <td className={td}>{f.onUpdate ?? <Dash />}</td>
                  <td className={td}>{f.onDelete ?? <Dash />}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </Section>
      )}
    </div>
  );
}

import { Handle, Position, type Node, type NodeProps } from '@xyflow/react';
import { ChevronDown, ChevronRight, KeyRound, Link2 } from 'lucide-react';
import { memo } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/cn';
import { HEADER_HEIGHT, ROW_HEIGHT } from './geometry';
import { type ViewTable, tableLabel } from './model';

export interface TableNodeData extends Record<string, unknown> {
  /** `undefined` for a table of the saved layout that no longer exists. */
  table?: ViewTable;
  /** Label of a missing table. */
  ghostLabel?: string;
  collapsed: boolean;
  color?: string;
  showTypes: boolean;
  /** Not part of the search / highlighted neighbourhood. */
  dim: boolean;
  /** Matches the search text. */
  match: boolean;
  status: 'ok' | 'new' | 'changed' | 'missing';
  width: number;
  height: number;
  onToggle: (key: string) => void;
}

export type TableFlowNode = Node<TableNodeData, 'table'>;

const HEX = /^#(?:[0-9a-f]{3}|[0-9a-f]{6})$/i;

function TableNodeView({ id, data, selected }: NodeProps<TableFlowNode>) {
  const { t } = useTranslation('erd');
  const ghost = data.table === undefined;
  const custom = data.color && HEX.test(data.color) ? data.color : undefined;
  const title = data.table ? tableLabel(data.table.table) : (data.ghostLabel ?? '');
  const isView = data.table !== undefined && data.table.table.kind !== 'table';
  return (
    <div
      data-testid={`erd-node-${data.table?.table.name ?? data.ghostLabel ?? id}`}
      data-status={data.status}
      style={{ width: data.width, height: data.height }}
      className={cn(
        'overflow-hidden rounded-md border bg-surface text-[12px] text-foreground shadow-sm',
        selected ? 'border-accent ring-1 ring-accent' : 'border-border',
        data.match && 'ring-2 ring-warning',
        data.dim && 'opacity-35',
        ghost && 'border-dashed border-danger bg-danger-soft',
      )}
    >
      <Handle type="target" position={Position.Left} style={{ opacity: 0 }} isConnectable={false} />
      <Handle
        type="source"
        position={Position.Right}
        style={{ opacity: 0 }}
        isConnectable={false}
      />
      <div
        style={{
          height: HEADER_HEIGHT,
          ...(custom ? { backgroundColor: custom, color: '#fff' } : {}),
        }}
        className={cn(
          'flex items-center gap-1 px-1.5 font-semibold',
          !custom && 'bg-surface-muted',
        )}
      >
        {data.table && (
          <button
            type="button"
            className="nodrag rounded p-0.5 opacity-70 hover:opacity-100"
            aria-label={data.collapsed ? t('node.expand') : t('node.collapse')}
            onClick={(e) => {
              e.stopPropagation();
              data.onToggle(id);
            }}
          >
            {data.collapsed ? <ChevronRight size={12} /> : <ChevronDown size={12} />}
          </button>
        )}
        <span className="min-w-0 flex-1 truncate" title={title}>
          {title}
        </span>
        {isView && <span className="text-[10px] font-normal opacity-70">{t('node.view')}</span>}
        {ghost && (
          <span className="text-[10px] font-medium text-danger">{t('status.missing')}</span>
        )}
        {data.status === 'new' && (
          <span className="rounded bg-success-soft px-1 text-[10px] font-medium text-success">
            {t('status.new')}
          </span>
        )}
        {data.status === 'changed' && (
          <span
            className="rounded bg-warning-soft px-1 text-[10px] font-medium text-warning"
            title={t('status.changedHint')}
          >
            {t('status.changed')}
          </span>
        )}
      </div>
      {data.table &&
        !data.collapsed &&
        data.table.columns.map((c, i) => (
          <div
            key={c.detail.name}
            style={{ height: ROW_HEIGHT }}
            className={cn('flex items-center gap-1 px-2', i > 0 && 'border-t border-border/50')}
            title={c.detail.comment ?? undefined}
          >
            <span className="flex w-5 shrink-0 justify-center text-muted-foreground">
              {c.pk ? (
                <KeyRound size={11} className="text-warning" aria-label="PK" />
              ) : c.fk ? (
                <Link2 size={11} className="text-accent" aria-label="FK" />
              ) : null}
            </span>
            <span
              className={cn(
                'min-w-0 flex-1 truncate',
                c.pk && 'font-semibold',
                c.detail.nullable && 'text-muted-foreground',
              )}
            >
              {c.detail.name}
            </span>
            {data.showTypes && (
              <span className="max-w-[45%] shrink-0 truncate text-[10.5px] text-muted-foreground">
                {c.detail.typeName}
              </span>
            )}
          </div>
        ))}
    </div>
  );
}

export const TableNode = memo(TableNodeView);

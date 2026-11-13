import { ChevronDown, ChevronRight } from 'lucide-react';
import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/cn';
import { RawNumber } from './format';
import {
  MAX_CHILDREN,
  MAX_EXPANDED_NODES,
  childEntries,
  collectExpandPaths,
  isContainer,
} from './treeModel';

export type TreeMode = 'default' | 'all' | 'none';

interface NodeProps {
  name: string | null;
  value: unknown;
  depth: number;
  path: string;
  mode: TreeMode;
  expanded: Set<string> | null;
}

function TreeNode({ name, value, depth, path, mode, expanded }: NodeProps) {
  const { t } = useTranslation('data');
  const [open, setOpen] = useState(
    mode === 'all' ? (expanded?.has(path) ?? false) : mode === 'none' ? false : depth < 2,
  );
  const label =
    name !== null ? (
      <span className="text-foreground">
        {name}
        <span className="text-muted-foreground">: </span>
      </span>
    ) : null;

  if (!isContainer(value)) {
    // Numbers (RawNumber) show their original text, without quotes.
    const tone =
      typeof value === 'string'
        ? 'text-success'
        : value instanceof RawNumber
          ? 'text-warning'
          : 'text-accent';
    return (
      <div className="flex pl-4 break-all">
        {label}
        <span className={cn(tone, value === null && 'italic')}>
          {typeof value === 'string'
            ? JSON.stringify(value)
            : value instanceof RawNumber
              ? value.raw
              : String(value)}
        </span>
      </div>
    );
  }

  const entries = childEntries(value);
  const isArray = Array.isArray(value);
  const summary = isArray
    ? t('viewer.items', { count: entries.length })
    : t('viewer.keys', { count: entries.length });
  return (
    <div>
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
        className="flex items-center gap-0.5 rounded hover:bg-surface-muted"
      >
        {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
        {label}
        <span className="text-muted-foreground">
          {isArray ? '[' : '{'} {entries.length === 0 ? t('viewer.empty') : summary}{' '}
          {isArray ? ']' : '}'}
        </span>
      </button>
      {open && entries.length > 0 && (
        <div className="ml-2 border-l border-border pl-2">
          {entries.slice(0, MAX_CHILDREN).map(([k, v]) => (
            <TreeNode
              key={k}
              name={k}
              value={v}
              depth={depth + 1}
              path={`${path}\u0000${k}`}
              mode={mode}
              expanded={expanded}
            />
          ))}
          {entries.length > MAX_CHILDREN && (
            <div className="pl-4 text-muted-foreground">
              {'…'} {entries.length - MAX_CHILDREN}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

/** Collapsible JSON tree. Change `epoch` together with `mode` to force every node open/closed. */
export function JsonTree({
  value,
  mode = 'default',
  epoch = 0,
}: {
  value: unknown;
  mode?: TreeMode;
  epoch?: number;
}) {
  const { t } = useTranslation('data');
  const expand = useMemo(
    () => (mode === 'all' ? collectExpandPaths(value) : null),
    [value, mode, epoch], // eslint-disable-line react-hooks/exhaustive-deps
  );
  return (
    <div className="font-mono text-xs leading-5">
      {expand?.truncated && (
        <div className="mb-1 text-warning">
          {t('viewer.expandLimit', { count: MAX_EXPANDED_NODES })}
        </div>
      )}
      <TreeNode
        key={epoch}
        name={null}
        value={value}
        depth={0}
        path=""
        mode={mode}
        expanded={expand?.paths ?? null}
      />
    </div>
  );
}

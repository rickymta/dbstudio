import { ChevronDown, ChevronRight } from 'lucide-react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/cn';

export type TreeMode = 'default' | 'all' | 'none';

const MAX_CHILDREN = 500;

function isContainer(v: unknown): v is Record<string, unknown> | unknown[] {
  return typeof v === 'object' && v !== null;
}

function childEntries(v: Record<string, unknown> | unknown[]): [string, unknown][] {
  return Array.isArray(v)
    ? v.map((x, i) => [String(i), x] as [string, unknown])
    : Object.entries(v);
}

interface NodeProps {
  name: string | null;
  value: unknown;
  depth: number;
  mode: TreeMode;
}

function TreeNode({ name, value, depth, mode }: NodeProps) {
  const { t } = useTranslation('data');
  const [open, setOpen] = useState(mode === 'all' ? true : mode === 'none' ? false : depth < 2);
  const label =
    name !== null ? (
      <span className="text-foreground">
        {name}
        <span className="text-muted-foreground">: </span>
      </span>
    ) : null;

  if (!isContainer(value)) {
    const tone =
      typeof value === 'string'
        ? 'text-success'
        : typeof value === 'number'
          ? 'text-warning'
          : 'text-accent';
    return (
      <div className="flex pl-4 break-all">
        {label}
        <span className={cn(tone, value === null && 'italic')}>
          {typeof value === 'string' ? JSON.stringify(value) : String(value)}
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
            <TreeNode key={k} name={k} value={v} depth={depth + 1} mode={mode} />
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
  return (
    <div className="font-mono text-xs leading-5" role="tree">
      <TreeNode key={epoch} name={null} value={value} depth={0} mode={mode} />
    </div>
  );
}

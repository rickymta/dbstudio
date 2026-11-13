import { Check, Copy, PanelBottom, PanelRight, X } from 'lucide-react';
import { useMemo, useState, type PointerEvent as ReactPointerEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { IconButton } from '@/components/ui/IconButton';
import { cn } from '@/lib/cn';
import type { ColumnMeta, Value } from '@/lib/types';
import {
  base64ByteLength,
  decodeBase64,
  detectImageMime,
  formatBytes,
  hexDump,
  valueToPlain,
  valueToText,
} from './format';
import type { GridModel } from './gridModel';
import { JsonTree, type TreeMode } from './JsonTree';
import { useTabUi } from './store';
import { useGridVersion } from './useGridVersion';

const DUMP_LIMIT = 64 * 1024;
const MIN_SIZE = 200;
const MAX_SIZE = 900;

function useCopy() {
  const [copied, setCopied] = useState(false);
  const copy = (text: string) => {
    void navigator.clipboard
      ?.writeText(text)
      .then(() => {
        setCopied(true);
        setTimeout(() => setCopied(false), 1200);
      })
      .catch(() => undefined);
  };
  return [copied, copy] as const;
}

/** Full-value rendering by kind. Exported for tests. */
export function ValueView({ value }: { value: Value }) {
  const { t } = useTranslation('data');
  switch (value.t) {
    case 'null':
      return (
        <div className="m-3 rounded-md border border-dashed border-border p-3">
          <div className="font-mono text-xs font-medium text-muted-foreground italic">
            {t('viewer.null')}
          </div>
          <div className="mt-1 text-xs text-muted-foreground">{t('viewer.nullHint')}</div>
        </div>
      );
    case 'json':
    case 'array':
    case 'document':
      return <StructuredView plain={value.t === 'json' ? value.v : valueToPlain(value)} />;
    case 'bytes':
      return <BytesView b64={value.v} />;
    case 'text':
      return <TextView text={value.v} />;
    default:
      return (
        <pre className="m-0 p-3 font-mono text-xs break-all whitespace-pre-wrap select-text">
          {valueToText(value)}
        </pre>
      );
  }
}

function TextView({ text }: { text: string }) {
  const { t } = useTranslation('data');
  const [wrap, setWrap] = useState(true);
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex h-7 shrink-0 items-center gap-3 border-b border-border px-3 text-xs text-muted-foreground">
        <span>{t('viewer.chars', { count: text.length })}</span>
        <label className="flex items-center gap-1">
          <input type="checkbox" checked={wrap} onChange={(e) => setWrap(e.target.checked)} />
          {t('viewer.wrap')}
        </label>
      </div>
      {text === '' ? (
        <div className="m-3 text-xs text-muted-foreground italic">{t('viewer.emptyString')}</div>
      ) : (
        <pre
          className={cn(
            'm-0 min-h-0 flex-1 overflow-auto p-3 font-mono text-xs select-text',
            wrap ? 'break-words whitespace-pre-wrap' : 'whitespace-pre',
          )}
        >
          {text}
        </pre>
      )}
    </div>
  );
}

function StructuredView({ plain }: { plain: unknown }) {
  const { t } = useTranslation('data');
  const [view, setView] = useState<'tree' | 'text'>('tree');
  const [mode, setMode] = useState<TreeMode>('default');
  const [epoch, setEpoch] = useState(0);
  const pretty = useMemo(() => {
    try {
      return JSON.stringify(plain, null, 2) ?? 'null';
    } catch {
      return String(plain);
    }
  }, [plain]);
  const setAll = (m: TreeMode) => {
    setMode(m);
    setEpoch((e) => e + 1);
  };
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex h-7 shrink-0 items-center gap-2 border-b border-border px-3 text-xs">
        {(['tree', 'text'] as const).map((v) => (
          <button
            key={v}
            type="button"
            aria-pressed={view === v}
            onClick={() => setView(v)}
            className={cn(
              'rounded px-1.5',
              view === v ? 'bg-accent text-accent-foreground' : 'text-muted-foreground',
            )}
          >
            {t(`viewer.${v}`)}
          </button>
        ))}
        {view === 'tree' && (
          <>
            <button
              type="button"
              className="text-muted-foreground hover:text-foreground"
              onClick={() => setAll('all')}
            >
              {t('viewer.expandAll')}
            </button>
            <button
              type="button"
              className="text-muted-foreground hover:text-foreground"
              onClick={() => setAll('none')}
            >
              {t('viewer.collapseAll')}
            </button>
          </>
        )}
      </div>
      <div className="min-h-0 flex-1 overflow-auto p-3 select-text">
        {view === 'tree' ? (
          <JsonTree value={plain} mode={mode} epoch={epoch} />
        ) : (
          <pre className="m-0 font-mono text-xs whitespace-pre-wrap">{pretty}</pre>
        )}
      </div>
    </div>
  );
}

function BytesView({ b64 }: { b64: string }) {
  const { t } = useTranslation('data');
  const bytes = useMemo(() => decodeBase64(b64), [b64]);
  const size = base64ByteLength(b64);
  const mime = detectImageMime(bytes);
  const dump = useMemo(() => hexDump(bytes, DUMP_LIMIT), [bytes]);
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex h-7 shrink-0 items-center gap-3 border-b border-border px-3 text-xs text-muted-foreground">
        <span>{t('viewer.bytes', { size: formatBytes(size), count: size })}</span>
        {mime && <span>{mime}</span>}
      </div>
      <div className="min-h-0 flex-1 overflow-auto p-3 select-text">
        {mime && (
          <div className="mb-3">
            <div className="mb-1 text-xs text-muted-foreground">{t('viewer.imagePreview')}</div>
            <img
              alt={t('viewer.imagePreview')}
              src={`data:${mime};base64,${b64}`}
              className="max-h-64 max-w-full rounded border border-border bg-surface-muted object-contain"
            />
          </div>
        )}
        <div className="mb-1 text-xs text-muted-foreground">{t('viewer.hexDump')}</div>
        <pre className="m-0 font-mono text-[11px] leading-4 whitespace-pre">{dump}</pre>
        {bytes.length > DUMP_LIMIT && (
          <div className="mt-1 text-xs text-muted-foreground">
            {t('viewer.dumpTruncated', { shown: DUMP_LIMIT, total: bytes.length })}
          </div>
        )}
      </div>
    </div>
  );
}

interface CellViewerProps {
  tabId: string;
  model: GridModel;
}

/** Side/bottom panel showing the full value of the selected cell. */
export function CellViewer({ tabId, model }: CellViewerProps) {
  const { t } = useTranslation('data');
  useGridVersion(model);
  const [ui, patchUi] = useTabUi(tabId);
  const [copied, copy] = useCopy();
  const sel = ui.selection;
  const value = sel ? model.rows[sel.row]?.[sel.col] : undefined;
  const column: ColumnMeta | undefined = sel ? model.columns[sel.col] : undefined;
  const right = ui.viewerPosition === 'right';

  const startResize = (e: ReactPointerEvent) => {
    e.preventDefault();
    const start = right ? e.clientX : e.clientY;
    const startSize = ui.viewerSize;
    const move = (ev: PointerEvent) => {
      const delta = (right ? ev.clientX : ev.clientY) - start;
      patchUi({ viewerSize: Math.min(MAX_SIZE, Math.max(MIN_SIZE, startSize - delta)) });
    };
    const up = () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  };

  return (
    <aside
      aria-label={t('viewer.title')}
      className={cn(
        'relative flex shrink-0 flex-col border-border bg-surface',
        right ? 'border-l' : 'border-t',
      )}
      style={right ? { width: ui.viewerSize } : { height: ui.viewerSize }}
    >
      <div
        role="separator"
        aria-orientation={right ? 'vertical' : 'horizontal'}
        aria-label={t('viewer.resize')}
        onPointerDown={startResize}
        className={cn(
          'absolute z-10 hover:bg-accent/40',
          right
            ? 'top-0 left-0 h-full w-1 cursor-col-resize'
            : 'top-0 left-0 h-1 w-full cursor-row-resize',
        )}
      />
      <div className="flex h-8 shrink-0 items-center gap-1 border-b border-border bg-surface-muted px-2">
        <div className="min-w-0 flex-1 truncate text-xs">
          <span className="font-medium">{column?.name ?? t('viewer.title')}</span>
          {column && <span className="ml-2 text-muted-foreground">{column.typeName}</span>}
          {sel && <span className="ml-2 text-muted-foreground">#{sel.row + 1}</span>}
        </div>
        <Button
          size="sm"
          variant="ghost"
          disabled={!value}
          onClick={() => value && copy(valueToText(value))}
        >
          {copied ? <Check size={12} /> : <Copy size={12} />}
          {copied ? t('viewer.copied') : t('viewer.copy')}
        </Button>
        <IconButton
          label={right ? t('viewer.dockBottom') : t('viewer.dockRight')}
          icon={right ? <PanelBottom size={13} /> : <PanelRight size={13} />}
          onClick={() => patchUi({ viewerPosition: right ? 'bottom' : 'right' })}
        />
        <IconButton
          label={t('viewer.close')}
          icon={<X size={13} />}
          onClick={() => patchUi({ viewerOpen: false })}
        />
      </div>
      {value ? (
        // Remount per cell so per-value view state (wrap, tree mode) does not leak across cells.
        <div key={`${sel?.row}:${sel?.col}`} className="flex min-h-0 flex-1 flex-col overflow-auto">
          <ValueView value={value} />
        </div>
      ) : (
        <div className="p-3 text-xs text-muted-foreground">{t('viewer.noSelection')}</div>
      )}
    </aside>
  );
}

import { Braces, FileCode2, Layers } from 'lucide-react';
import type { FC, KeyboardEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/cn';
import type { DebugFrame, FrameKind } from '@/lib/types';
import { selectFrame } from './debugActions';
import { useDebugStore } from './debugStore';

const ICONS: Record<FrameKind, typeof Layers> = {
  script: FileCode2,
  block: Braces,
  routine: Layers,
};

/** Call stack: innermost frame first; selecting a frame re-scopes Variables/Watches/inline. */
export const FramesPanel: FC<{ tabId: string }> = ({ tabId }) => {
  const { t } = useTranslation('debugger');
  const state = useDebugStore((s) => s.sessions[tabId]?.state);
  const frames = state?.frames ?? [];
  const selected = state?.selectedFrame ?? 0;

  const onKey = (e: KeyboardEvent, frame: DebugFrame) => {
    const index = frames.findIndex((f) => f.id === frame.id);
    const target =
      e.key === 'ArrowDown'
        ? frames[index + 1]
        : e.key === 'ArrowUp'
          ? frames[index - 1]
          : undefined;
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      void selectFrame(tabId, frame.id);
    } else if (target) {
      e.preventDefault();
      void selectFrame(tabId, target.id);
      (
        e.currentTarget.parentElement?.querySelector(
          `[data-frame="${target.id}"]`,
        ) as HTMLElement | null
      )?.focus();
    }
  };

  return (
    <section className="flex h-full min-h-0 flex-col" aria-label={t('frames.title')}>
      <header className="shrink-0 border-b border-border px-2 py-1 text-[11px] font-medium text-muted-foreground uppercase">
        {t('frames.title')}
      </header>
      <div
        role="tree"
        aria-label={t('frames.title')}
        className="min-h-0 flex-1 overflow-auto py-0.5"
      >
        {frames.length === 0 && (
          <p className="px-2 py-1 text-xs text-muted-foreground">{t('frames.empty')}</p>
        )}
        {frames.map((frame) => {
          const Icon = ICONS[frame.kind];
          const active = frame.id === selected;
          return (
            <div
              key={frame.id}
              role="treeitem"
              aria-selected={active}
              aria-level={1}
              tabIndex={active ? 0 : -1}
              data-frame={frame.id}
              className={cn(
                'flex cursor-default items-center gap-1.5 px-2 py-0.5 text-[13px] outline-none',
                'focus-visible:ring-1 focus-visible:ring-ring',
                active ? 'bg-accent/15 font-medium' : 'hover:bg-surface-muted',
              )}
              onClick={() => void selectFrame(tabId, frame.id)}
              onKeyDown={(e) => onKey(e, frame)}
            >
              <Icon size={13} className="shrink-0 text-muted-foreground" aria-hidden />
              <span
                className="min-w-0 truncate"
                title={frame.args ? `${frame.name}(${frame.args})` : frame.name}
              >
                {frame.name}
                <span className="text-muted-foreground">:{frame.line}</span>
              </span>
              {frame.kind === 'block' && (
                <span className="shrink-0 text-xs text-muted-foreground">
                  {t('frames.notSteppable')}
                </span>
              )}
            </div>
          );
        })}
      </div>
    </section>
  );
};

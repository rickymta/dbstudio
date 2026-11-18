import { Fragment, type ReactNode } from 'react';
import { cn } from '@/lib/cn';
import { matchRanges } from '@/lib/text';

/** `text` with every occurrence of `query` marked (sidebar search, BROWSE-06). */
export function Highlight({
  text,
  query,
  className,
}: {
  text: string;
  query?: string;
  className?: string;
}) {
  const ranges = matchRanges(text, query ?? '');
  if (ranges.length === 0) return <>{text}</>;
  const parts: ReactNode[] = [];
  let pos = 0;
  for (const [start, end] of ranges) {
    if (start > pos) parts.push(<Fragment key={`t${pos}`}>{text.slice(pos, start)}</Fragment>);
    parts.push(
      <mark
        key={`m${start}`}
        data-search-match
        className={cn(
          'rounded-[2px] bg-warning/50 font-semibold text-foreground ring-1 ring-warning/70',
          className,
        )}
      >
        {text.slice(start, end)}
      </mark>,
    );
    pos = end;
  }
  if (pos < text.length) parts.push(<Fragment key={`t${pos}`}>{text.slice(pos)}</Fragment>);
  return <>{parts}</>;
}

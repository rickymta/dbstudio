import type { ReactNode } from 'react';
import type { StatementResult } from '@/features/editor/results';
import { formatReplyRows } from './replyFormat';

const MAX_LINES = 500;

/** Typed reply of a Redis command, drawn like `redis-cli` prints it. */
export function ReplyView({ lines }: { lines: string[] }) {
  return (
    <pre
      className="font-mono text-xs break-words whitespace-pre-wrap select-text"
      data-testid="redis-reply"
    >
      {lines.slice(0, MAX_LINES).join('\n')}
      {lines.length > MAX_LINES && `\n... (${lines.length - MAX_LINES})`}
    </pre>
  );
}

/** Console renderer for Redis: `null` (default rendering) for errors and statements without rows. */
export function renderRedisReply(result: StatementResult): ReactNode | null {
  const rows = result.model?.rows;
  if (result.error || !rows || rows.length === 0) return null;
  return <ReplyView lines={formatReplyRows(rows)} />;
}

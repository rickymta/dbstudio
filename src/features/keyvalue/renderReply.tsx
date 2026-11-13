import type { ReactNode } from 'react';
import type { StatementResult } from '@/features/editor/results';
import { formatReplyRows } from './replyFormat';
import { ReplyView } from './ReplyView';

/** Console renderer for Redis: `null` (default rendering) for errors and statements without rows. */
export function renderRedisReply(result: StatementResult): ReactNode | null {
  const rows = result.model?.rows;
  if (result.error || !rows || rows.length === 0) return null;
  return <ReplyView lines={formatReplyRows(rows)} />;
}

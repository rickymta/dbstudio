// Transaction bar (QUERY-11): auto-commit switch, state of the transaction, begin / commit /
// rollback. Also tells the user when the connection was lost.

import { AlertTriangle, Check, Play, Undo2, X } from 'lucide-react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/Badge';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Switch } from '@/components/ui/Switch';
import type { Uuid } from '@/lib/types';
import { beginTransaction, endTransaction, setAutoCommit, ensureLease } from './editorRunner';
import { hasOpenTransaction, patchTabData, type EditorTabData } from './editorStore';

export interface TxBarProps {
  data: EditorTabData;
  /** `null` while the connection is closed: the bar only shows the preference. */
  sessionId: Uuid | null;
  running: boolean;
}

export function TxBar({ data, sessionId, running }: TxBarProps) {
  const { t } = useTranslation('editor');
  const [deciding, setDeciding] = useState(false);
  const state = data.leaseState;
  const autoCommit = state?.autoCommit ?? data.autoCommit;
  const tx = state?.transaction ?? 'idle';
  const open = hasOpenTransaction(data);
  const busy = running || !sessionId;
  const id = data.id;

  const toggle = async (on: boolean) => {
    const r = await setAutoCommit(id, sessionId, on);
    if (r === 'needsDecision') setDeciding(true);
  };
  const decide = async (how: 'commit' | 'rollback') => {
    setDeciding(false);
    if (!sessionId) return;
    if (await endTransaction(id, sessionId, how)) await setAutoCommit(id, sessionId, true);
  };

  return (
    <div className="shrink-0 border-b border-border bg-surface" data-testid="tx-bar">
      <div className="flex h-8 items-center gap-3 px-2 text-xs">
        <label className="flex items-center gap-1.5">
          <Switch
            checked={autoCommit}
            disabled={busy && !!sessionId}
            onCheckedChange={(on) => void toggle(on)}
            aria-label={t('tx.autoCommit')}
          />
          {t('tx.autoCommit')}
        </label>
        <TxBadge tx={tx} autoCommit={autoCommit} />
        <span className="ml-auto flex items-center gap-1">
          {autoCommit && !open && (
            <Button
              size="sm"
              disabled={busy}
              onClick={() => sessionId && void beginTransaction(id, sessionId)}
            >
              <Play size={12} aria-hidden /> {t('tx.begin')}
            </Button>
          )}
          <Button
            size="sm"
            disabled={busy || !open || tx === 'failed'}
            onClick={() => sessionId && void endTransaction(id, sessionId, 'commit')}
          >
            <Check size={12} aria-hidden /> {t('tx.commit')}
          </Button>
          <Button
            size="sm"
            disabled={busy || !open}
            onClick={() => sessionId && void endTransaction(id, sessionId, 'rollback')}
          >
            <Undo2 size={12} aria-hidden /> {t('tx.rollback')}
          </Button>
        </span>
      </div>
      {data.transactionLost && (
        <Banner tone="warning" onDismiss={() => patchTabData(id, { transactionLost: false })}>
          {t('tx.lost')}
        </Banner>
      )}
      {state?.broken && sessionId && (
        <Banner
          tone="danger"
          action={
            <Button
              size="sm"
              onClick={() => void ensureLease(id, sessionId).catch(() => undefined)}
            >
              {t('connection.reconnect')}
            </Button>
          }
        >
          {t('connection.broken')}
        </Banner>
      )}
      <Dialog open={deciding} onOpenChange={(o) => !o && setDeciding(false)}>
        <DialogContent
          title={t('tx.decideTitle')}
          description={t('tx.decideDescription')}
          footer={
            <>
              <Button onClick={() => setDeciding(false)}>{t('tx.cancel')}</Button>
              <Button onClick={() => void decide('rollback')}>{t('tx.rollbackAndSwitch')}</Button>
              <Button variant="primary" onClick={() => void decide('commit')}>
                {t('tx.commitAndSwitch')}
              </Button>
            </>
          }
        />
      </Dialog>
    </div>
  );
}

export function TxBadge({
  tx,
  autoCommit,
}: {
  tx: 'idle' | 'active' | 'failed' | 'unknown';
  autoCommit: boolean;
}) {
  const { t } = useTranslation('editor');
  const tone = tx === 'active' ? 'warning' : tx === 'failed' ? 'warning' : 'neutral';
  const text =
    tx === 'idle'
      ? autoCommit
        ? t('tx.status.idleAuto')
        : t('tx.status.idleManual')
      : t(`tx.status.${tx}`);
  return (
    <Badge tone={tone} data-testid="tx-state" data-tx={tx}>
      {text}
    </Badge>
  );
}

function Banner({
  tone,
  children,
  action,
  onDismiss,
}: {
  tone: 'warning' | 'danger';
  children: React.ReactNode;
  action?: React.ReactNode;
  onDismiss?: () => void;
}) {
  const { t } = useTranslation('editor');
  return (
    <div
      role="alert"
      className={`flex items-center gap-2 border-t border-border px-2 py-1 text-xs ${
        tone === 'danger' ? 'bg-danger-soft text-danger' : 'bg-warning-soft text-warning'
      }`}
    >
      <AlertTriangle size={12} aria-hidden />
      <span className="min-w-0 flex-1">{children}</span>
      {action}
      {onDismiss && (
        <button type="button" aria-label={t('tx.dismiss')} onClick={onDismiss}>
          <X size={12} />
        </button>
      )}
    </div>
  );
}

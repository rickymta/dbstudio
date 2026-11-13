import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';
import { Spinner } from '@/components/ui/Spinner';
import { ipc, toIpcError } from '@/lib/ipc';
import { askConfirmation } from '@/features/safety/confirmStore';
import { guardedCall } from '@/features/safety/useGuardedCall';
import type {
  ConfirmationGrant,
  ConfirmationRequest,
  IpcError,
  JobEvent,
  JobProgress,
  JobStatus,
  ObjectNode,
  Uuid,
} from '@/lib/types';
import { databaseOfKeyPath, defaultDeletePattern } from './keyDetail';

interface DeleteByPatternDialogProps {
  open: boolean;
  sessionId: Uuid;
  /** Database or key-prefix node the dialog was opened on. */
  node: ObjectNode;
  onOpenChange: (open: boolean) => void;
  /** The job ended (any status); the tree should reload. */
  onDone?: () => void;
}

type Stage = { name: 'input' } | { name: 'running'; jobId: string };

interface JobView {
  status: JobStatus;
  progress: JobProgress | null;
  message: string | null;
}

/**
 * "Delete keys by pattern": pattern input, job progress. The typed confirmation goes through the
 * shared destructive dialog (`ConfirmDestructiveHost`).
 */
export function DeleteByPatternDialog({
  open,
  sessionId,
  node,
  onOpenChange,
  onDone,
}: DeleteByPatternDialogProps) {
  const { t } = useTranslation('keyvalue');
  const [pattern, setPattern] = useState(() => defaultDeletePattern(node));
  const [stage, setStage] = useState<Stage>({ name: 'input' });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<IpcError | null>(null);
  const [jobs, setJobs] = useState<Record<string, JobView>>({});

  // Job events can arrive before the start call returns, so keep every key-delete job's state.
  useEffect(() => {
    let unlisten: (() => void) | null = null;
    let disposed = false;
    void ipc
      .onEvent<JobEvent>('job://progress', (event) => {
        setJobs((prev) => {
          if (event.type === 'progress') {
            const old = prev[event.jobId];
            return {
              ...prev,
              [event.jobId]: {
                status: old?.status ?? 'running',
                progress: event.progress,
                message: event.progress.message,
              },
            };
          }
          if (event.job.kind !== 'keyDelete') return prev;
          return {
            ...prev,
            [event.job.id]: {
              status: event.job.status,
              progress: event.job.progress,
              message: event.job.error?.message ?? event.job.progress.message,
            },
          };
        });
      })
      .then((off) => {
        if (disposed) off();
        else unlisten = off;
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  const jobId = stage.name === 'running' ? stage.jobId : null;
  const job = jobId ? jobs[jobId] : undefined;
  const finished = job !== undefined && job.status !== 'running' && job.status !== 'queued';

  useEffect(() => {
    if (finished) onDone?.();
    // `onDone` is intentionally not a dependency: it must run once per finished job.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [finished]);

  const start = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const database = databaseOfKeyPath(node.path);
      // A pattern that matches the whole database needs a typed confirmation on any
      // connection; on production the backend asks for the same text, so it is not asked twice.
      const wholeDb = /^\*+$/.test(pattern.trim());
      const local: ConfirmationRequest = {
        token: '',
        confirmText: `db${database.segments[0]?.name ?? '0'}`,
        statements: [
          {
            index: 0,
            range: { start: 0, end: pattern.length },
            text: `DELETE KEYS ${pattern}`,
            reason: 'flushDb',
          },
        ],
        expiresInMs: 0,
      };
      let localTyped = false;
      const ask = async (request: ConfirmationRequest): Promise<ConfirmationGrant> =>
        localTyped && request.confirmText === local.confirmText
          ? { token: request.token, typed: request.confirmText }
          : askConfirmation(request);
      const id = await guardedCall(async (grant) => {
        if (wholeDb && !localTyped) {
          await askConfirmation(local);
          localTyped = true;
        }
        return ipc.keysDeletePattern(sessionId, database, pattern, grant ?? null);
      }, ask);
      setStage({ name: 'running', jobId: id });
    } catch (e) {
      const err = toIpcError(e);
      // Declining the confirmation just returns to the pattern input.
      if (err.kind !== 'cancelled') setError(err);
    } finally {
      setBusy(false);
    }
  }, [sessionId, node.path, pattern]);

  const cancelJob = () => {
    if (!jobId) return;
    ipc.jobCancel(jobId).catch((e: unknown) => setError(toIpcError(e)));
  };

  let body;
  let footer;
  if (stage.name === 'input') {
    body = (
      <div className="flex flex-col gap-2">
        <Input
          autoFocus
          aria-label={t('deletePattern.pattern')}
          value={pattern}
          onChange={(e) => setPattern(e.target.value)}
          className="font-mono"
        />
        <p className="text-xs text-muted-foreground">{t('deletePattern.hint')}</p>
      </div>
    );
    footer = (
      <>
        <Button onClick={() => onOpenChange(false)}>{t('actions.cancel')}</Button>
        <Button
          variant="danger"
          disabled={busy || pattern.trim() === ''}
          onClick={() => void start()}
        >
          {t('deletePattern.start')}
        </Button>
      </>
    );
  } else {
    const done = job?.progress?.rowsDone ?? 0;
    const total = job?.progress?.rowsTotal ?? null;
    body = (
      <div className="flex flex-col gap-2 text-sm" data-testid="delete-pattern-progress">
        <div className="flex items-center gap-2">
          {!finished && <Spinner size={14} />}
          <span>
            {finished
              ? t(`deletePattern.status.${job.status}`, { count: done })
              : t('deletePattern.running', { done, total: total ?? '?' })}
          </span>
        </div>
        {job?.message && <p className="font-mono text-xs text-muted-foreground">{job.message}</p>}
      </div>
    );
    footer = finished ? (
      <Button variant="primary" onClick={() => onOpenChange(false)}>
        {t('actions.close')}
      </Button>
    ) : (
      <Button variant="danger" onClick={cancelJob}>
        {t('deletePattern.cancelJob')}
      </Button>
    );
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        title={t('deletePattern.title')}
        description={t('deletePattern.description')}
        className="w-[min(460px,92vw)]"
        footer={footer}
      >
        {body}
        {error && (
          <p role="alert" className="mt-2 text-xs text-danger">
            {error.message}
          </p>
        )}
      </DialogContent>
    </Dialog>
  );
}

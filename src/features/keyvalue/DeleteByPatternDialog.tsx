import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';
import { Spinner } from '@/components/ui/Spinner';
import { ipc, toIpcError } from '@/lib/ipc';
import type {
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

type Stage =
  | { name: 'input' }
  | { name: 'confirm'; request: ConfirmationRequest }
  | { name: 'running'; jobId: string };

interface JobView {
  status: JobStatus;
  progress: JobProgress | null;
  message: string | null;
}

/** "Delete keys by pattern": pattern input, typed confirmation when required, job progress. */
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
  const [typed, setTyped] = useState('');
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

  const start = useCallback(
    async (grant?: { token: string; typed: string }) => {
      // A pattern that matches the whole database needs a typed confirmation on any connection.
      if (!grant && /^\*+$/.test(pattern.trim())) {
        const name = databaseOfKeyPath(node.path).segments[0]?.name ?? '0';
        setTyped('');
        setStage({
          name: 'confirm',
          request: {
            token: '',
            confirmText: `db${name}`,
            statements: [],
            expiresInMs: 0,
          },
        });
        return;
      }
      setBusy(true);
      setError(null);
      try {
        const id = await ipc.keysDeletePattern(
          sessionId,
          databaseOfKeyPath(node.path),
          pattern,
          grant ?? null,
        );
        setStage({ name: 'running', jobId: id });
      } catch (e) {
        const err = toIpcError(e);
        if (err.kind === 'confirmationRequired' && err.confirmation) {
          setTyped('');
          setStage({ name: 'confirm', request: err.confirmation });
        } else {
          setError(err);
        }
      } finally {
        setBusy(false);
      }
    },
    [sessionId, node.path, pattern],
  );

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
  } else if (stage.name === 'confirm') {
    const { request } = stage;
    body = (
      <div className="flex flex-col gap-2">
        <p className="text-sm">{t('deletePattern.confirmPrompt', { pattern })}</p>
        <p className="font-mono text-xs">
          {t('deletePattern.typeToConfirm')} <strong>{request.confirmText}</strong>
        </p>
        <Input
          autoFocus
          aria-label={t('deletePattern.confirmInput')}
          value={typed}
          onChange={(e) => setTyped(e.target.value)}
          className="font-mono"
        />
      </div>
    );
    footer = (
      <>
        <Button onClick={() => onOpenChange(false)}>{t('actions.cancel')}</Button>
        <Button
          variant="danger"
          disabled={busy || typed !== request.confirmText}
          onClick={() => void start({ token: request.token, typed })}
        >
          {t('deletePattern.confirm')}
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

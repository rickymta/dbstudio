import { FileUp, ShieldCheck, TriangleAlert } from 'lucide-react';
import { useEffect, useId, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Checkbox } from '@/components/ui/Checkbox';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';
import { Select } from '@/components/ui/Select';
import { Spinner } from '@/components/ui/Spinner';
import { useConnectionsSnapshot } from '@/features/connections/queries';
import { describeError } from '@/features/connections/errors';
import { useGuardedCall } from '@/features/safety/useGuardedCall';
import { ipc, toIpcError } from '@/lib/ipc';
import type { BackupInspection, JobId, RestoreSpec } from '@/lib/types';
import { JobProgressView } from './JobProgress';
import { readInspection } from './inspection';
import { isActive, useBackupEvents } from './queries';

export interface RestoreDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Pre-selected backup file (e.g. from the run history). */
  initialFile?: string | null;
}

const BACKUP_FILTERS = [{ name: 'DBStudio backup', extensions: ['dbsbak'] }];

export function RestoreDialog(props: RestoreDialogProps) {
  return (
    <Dialog open={props.open} onOpenChange={props.onOpenChange}>
      {props.open && <RestoreBody {...props} />}
    </Dialog>
  );
}

function RestoreBody({ onOpenChange, initialFile = null }: RestoreDialogProps) {
  const { t } = useTranslation('backup');
  const id = useId();
  const jobs = useBackupEvents();
  const profiles = useConnectionsSnapshot().data?.profiles ?? [];

  const [file, setFile] = useState<string | null>(null);
  const [info, setInfo] = useState<BackupInspection | null>(null);
  const [inspecting, setInspecting] = useState(false);
  const [inspectError, setInspectError] = useState<string | null>(null);
  const [passphrase, setPassphrase] = useState('');
  const [passError, setPassError] = useState<string | null>(null);

  const [targetId, setTargetId] = useState('');
  const [database, setDatabase] = useState('');
  const [createDatabase, setCreateDatabase] = useState(false);
  const [clean, setClean] = useState(false);
  const [continueOnError, setContinueOnError] = useState(false);
  const [selected, setSelected] = useState<Set<string> | null>(null);

  const [starting, setStarting] = useState(false);
  const [startError, setStartError] = useState<string | null>(null);
  const [restoreJob, setRestoreJob] = useState<JobId | null>(null);
  const [verifyJob, setVerifyJob] = useState<JobId | null>(null);

  const view = info ? readInspection(info) : null;
  const locked = !!info && info.encrypted && !view?.readable;
  const target = profiles.find((p) => p.id === targetId);
  const objectsSel = selected ?? new Set(view?.objects ?? []);

  const inspect = async (path: string, pass: string | null) => {
    setInspecting(true);
    setInspectError(null);
    setPassError(null);
    try {
      const result = await ipc.backupInspect(path, pass);
      setInfo(result);
      const v = readInspection(result);
      if (v.sourceDatabase && !database) setDatabase(v.sourceDatabase);
    } catch (e) {
      const err = toIpcError(e);
      if (err.kind === 'integrity' && pass) {
        setPassError(t('restore.wrongPassphrase'));
      } else if (err.kind === 'integrity') {
        // The header is readable without the passphrase; the content is not.
        setInfo({
          header: { formatVersion: 1, compression: 'none', encryption: null, createdAt: '' },
          preamble: null,
          encrypted: true,
          native: null,
        });
      } else {
        setInfo(null);
        setInspectError(describeError(t, e));
      }
    } finally {
      setInspecting(false);
    }
  };

  const chooseFile = async (path?: string | null) => {
    const picked = path ?? (await ipc.filePickOpen(BACKUP_FILTERS, false));
    if (!picked) return;
    setFile(picked);
    setInfo(null);
    setSelected(null);
    setPassphrase('');
    setRestoreJob(null);
    setVerifyJob(null);
    await inspect(picked, null);
  };

  // Open the pre-selected file once, on mount.
  useEffect(() => {
    // Deferred so the state updates of the first inspection do not run inside the effect body.
    if (initialFile) void Promise.resolve().then(() => chooseFile(initialFile));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const buildSpec = (): RestoreSpec => {
    const all = view?.objects ?? [];
    // An integrated backup is one SQL script: it is restored as a whole.
    const subset =
      !view?.integrated && selected && selected.size < all.length ? [...selected] : undefined;
    return {
      file: file!,
      targetConnectionId: targetId,
      targetDatabase: database.trim() || null,
      createDatabase,
      clean,
      continueOnError,
      ...(subset ? { objects: subset } : {}),
    };
  };

  const guard = useGuardedCall();
  const start = async () => {
    setStarting(true);
    setStartError(null);
    try {
      // A production target (or `clean`) answers `confirmationRequired`: the shared dialog asks
      // for the typed text and the call is repeated with the grant.
      const spec = buildSpec();
      const jobId = await guard((grant) =>
        ipc.restoreStart(spec, passphrase || null, grant ?? null),
      );
      setRestoreJob(jobId);
    } catch (e) {
      const err = toIpcError(e);
      if (err.kind !== 'cancelled') setStartError(describeError(t, e));
    } finally {
      setStarting(false);
    }
  };

  const verify = async () => {
    if (!file) return;
    setStartError(null);
    try {
      setVerifyJob(await ipc.backupVerify(file, passphrase || null));
    } catch (e) {
      setStartError(describeError(t, e));
    }
  };

  const restoreInfo = restoreJob ? jobs[restoreJob] : undefined;
  const verifyInfo = verifyJob ? jobs[verifyJob] : undefined;
  const restoreActive = restoreJob !== null && (!restoreInfo || isActive(restoreInfo));
  const busy = starting || restoreActive || restoreInfo?.status === 'succeeded';
  // A production target or a clean restore must name the database it writes to.
  const needsDatabase = (clean || target?.environment === 'production') && !database.trim();
  const canRestore =
    !needsDatabase &&
    !!file &&
    !!info &&
    !!view &&
    !locked &&
    !!targetId &&
    !(info.encrypted && !passphrase) &&
    !(!view.integrated && view.objects.length > 0 && objectsSel.size === 0) &&
    !busy;

  return (
    <>
      <DialogContent
        title={t('restore.title')}
        description={t('restore.description')}
        className="w-[min(680px,95vw)]"
        footer={
          <>
            <Button onClick={() => onOpenChange(false)}>{t('actions.close')}</Button>
            <Button variant="primary" disabled={!canRestore} onClick={() => void start()}>
              {starting && <Spinner size={12} className="text-accent-foreground" />}
              {t('restore.start')}
            </Button>
          </>
        }
      >
        <div className="flex flex-col gap-4">
          {startError && (
            <p role="alert" className="rounded-md bg-danger-soft px-2 py-1.5 text-xs text-danger">
              {startError}
            </p>
          )}

          <Section title={t('restore.section.file')}>
            <div className="flex items-center gap-2">
              <Button onClick={() => void chooseFile()}>
                <FileUp size={14} aria-hidden /> {t('restore.chooseFile')}
              </Button>
              <span className="truncate font-mono text-xs" data-testid="restore-file">
                {file ?? t('restore.noFile')}
              </span>
              {inspecting && <Spinner size={12} />}
            </div>
            {inspectError && (
              <p role="alert" className="text-xs text-danger">
                {inspectError}
              </p>
            )}
            {info?.encrypted && (
              <div className="flex flex-col gap-1">
                <Label htmlFor={`${id}-pass`}>{t('restore.passphrase')}</Label>
                <div className="flex gap-2">
                  <Input
                    id={`${id}-pass`}
                    type="password"
                    autoComplete="off"
                    value={passphrase}
                    aria-invalid={!!passError || undefined}
                    onChange={(e) => setPassphrase(e.target.value)}
                  />
                  <Button
                    disabled={!passphrase || inspecting}
                    onClick={() => file && void inspect(file, passphrase)}
                  >
                    {t('restore.unlock')}
                  </Button>
                </div>
                <p className="text-[11px] text-muted-foreground">
                  {locked ? t('restore.encryptedLocked') : t('restore.encryptedHint')}
                </p>
                {passError && (
                  <p role="alert" className="text-xs text-danger">
                    {passError}
                  </p>
                )}
              </div>
            )}
          </Section>

          {info && view?.readable && (
            <Section title={t('restore.section.contents')}>
              <dl
                className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-xs"
                data-testid="restore-header"
              >
                {view.headerFields.map((f) => (
                  <div key={f.key} className="contents">
                    <dt className="text-muted-foreground">{f.key}</dt>
                    <dd className="font-mono">{f.value}</dd>
                  </div>
                ))}
                {info.native && (
                  <>
                    <dt className="text-muted-foreground">{t('restore.nativeTool')}</dt>
                    <dd className="font-mono">{t(`tools.name.${info.native}`)}</dd>
                  </>
                )}
              </dl>
              {view.objects.length > 0 && (
                <div className="flex flex-col gap-1">
                  <div className="flex items-center justify-between">
                    <Label>{t('restore.objects', { count: view.objects.length })}</Label>
                    {!view.integrated && (
                      <div className="flex gap-1">
                        <Button
                          size="sm"
                          variant="ghost"
                          onClick={() => setSelected(new Set(view.objects))}
                        >
                          {t('restore.selectAll')}
                        </Button>
                        <Button size="sm" variant="ghost" onClick={() => setSelected(new Set())}>
                          {t('restore.selectNone')}
                        </Button>
                      </div>
                    )}
                  </div>
                  <ul className="max-h-32 overflow-y-auto rounded-md border border-border p-1">
                    {view.objects.map((name) => (
                      <li key={name}>
                        <label className="flex items-center gap-2 px-1 py-0.5 text-xs">
                          {!view.integrated && (
                            <Checkbox
                              checked={objectsSel.has(name)}
                              aria-label={name}
                              onCheckedChange={(v) => {
                                const next = new Set(objectsSel);
                                if (v === true) next.add(name);
                                else next.delete(name);
                                setSelected(next);
                              }}
                            />
                          )}
                          <span className="font-mono">{name}</span>
                        </label>
                      </li>
                    ))}
                  </ul>
                </div>
              )}
              {view.extra && (
                <details className="text-xs">
                  <summary className="cursor-pointer text-muted-foreground">
                    {t('restore.scope')}
                  </summary>
                  <pre className="mt-1 max-h-32 overflow-auto rounded bg-surface-muted p-2 text-[11px]">
                    {view.extra}
                  </pre>
                </details>
              )}
            </Section>
          )}

          {info && !locked && (
            <Section title={t('restore.section.target')}>
              <div className="grid grid-cols-2 gap-2">
                <div className="flex flex-col gap-1">
                  <Label htmlFor={`${id}-target`}>{t('restore.targetConnection')}</Label>
                  <Select
                    id={`${id}-target`}
                    value={targetId || undefined}
                    onValueChange={setTargetId}
                    placeholder={t('job.connectionPlaceholder')}
                    options={profiles.map((p) => ({
                      value: p.id,
                      label: p.readOnly ? `${p.name} (${t('restore.readOnly')})` : p.name,
                      disabled: p.readOnly,
                    }))}
                  />
                </div>
                <div className="flex flex-col gap-1">
                  <Label htmlFor={`${id}-db`}>{t('restore.targetDatabase')}</Label>
                  <Input
                    id={`${id}-db`}
                    value={database}
                    onChange={(e) => setDatabase(e.target.value)}
                  />
                  {needsDatabase && (
                    <p className="text-xs text-danger" data-testid="restore-database-required">
                      {t('restore.targetRequired')}
                    </p>
                  )}
                </div>
              </div>
              {target?.environment === 'production' && (
                <p
                  className="flex items-center gap-1.5 text-xs text-warning"
                  data-testid="restore-production"
                >
                  <TriangleAlert size={14} aria-hidden /> {t('restore.productionWarning')}
                </p>
              )}
              <label className="flex items-center gap-2 text-[13px]">
                <Checkbox
                  checked={createDatabase}
                  onCheckedChange={(v) => setCreateDatabase(v === true)}
                  aria-label={t('restore.createDatabase')}
                />
                {t('restore.createDatabase')}
              </label>
              <label className="flex items-center gap-2 text-[13px]">
                <Checkbox
                  checked={clean}
                  onCheckedChange={(v) => setClean(v === true)}
                  aria-label={t('restore.clean')}
                />
                {t('restore.clean')}
              </label>
              {clean && (
                <p
                  role="note"
                  className="rounded-md bg-danger-soft px-2 py-1.5 text-xs text-danger"
                >
                  {t('restore.cleanWarning')}
                </p>
              )}
              {view?.dialect === 'mysql' && (
                <p className="text-xs text-muted-foreground" data-testid="restore-not-atomic">
                  {t('restore.mysqlNotAtomic')}
                </p>
              )}
              <label className="flex items-center gap-2 text-[13px]">
                <Checkbox
                  checked={continueOnError}
                  onCheckedChange={(v) => setContinueOnError(v === true)}
                  aria-label={t('restore.continueOnError')}
                />
                {t('restore.continueOnError')}
              </label>
            </Section>
          )}

          {info && !locked && (
            <Section title={t('restore.section.run')}>
              <div className="flex flex-wrap items-center gap-2">
                <Button
                  disabled={!file || (info.encrypted && !passphrase)}
                  onClick={() => void verify()}
                >
                  <ShieldCheck size={14} aria-hidden /> {t('restore.verify')}
                </Button>
              </div>
              {verifyInfo && <JobProgressView job={verifyInfo} />}
              {restoreInfo && <JobProgressView job={restoreInfo} />}
            </Section>
          )}
        </div>
      </DialogContent>
    </>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-2">
      <h3 className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
        {title}
      </h3>
      {children}
    </section>
  );
}

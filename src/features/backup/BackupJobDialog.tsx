import { useQuery, useQueryClient } from '@tanstack/react-query';
import { AlertTriangle, FolderOpen, ShieldAlert } from 'lucide-react';
import { useId, useMemo, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Checkbox } from '@/components/ui/Checkbox';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';
import { Select } from '@/components/ui/Select';
import { Spinner } from '@/components/ui/Spinner';
import { Switch } from '@/components/ui/Switch';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/Tabs';
import { toast } from '@/components/ui/toastStore';
import { connectionsQueryKey, useConnectionsSnapshot } from '@/features/connections/queries';
import { describeError } from '@/features/connections/errors';
import { ipc } from '@/lib/ipc';
import { ROOT_PATH } from '@/lib/objectPath';
import type {
  BackupJob,
  ConnectionProfile,
  ExportContent,
  MissedRunPolicy,
  NativeTool,
} from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { CronBuilder } from './CronBuilder';
import {
  buildJob,
  buildSpec,
  COMPRESSION_LEVELS,
  emptyForm,
  fileNameExample,
  firstErrorSection,
  formFromJob,
  NATIVE_TOOLS,
  passphraseChange,
  SERVER_SIDE_TOOLS,
  validateForm,
  FIELD_SECTION,
  type CompressionKind,
  type FormErrors,
  type JobForm,
} from './jobForm';
import { backupJobsKey, useSchedulePreview, useToolsQuery } from './queries';

export interface BackupJobDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Job to edit; `null` creates one. */
  job?: BackupJob | null;
  /** `once` runs a single backup now instead of saving a scheduled job. */
  mode?: 'job' | 'once';
  onDone?: () => void;
}

export function BackupJobDialog(props: BackupJobDialogProps) {
  return (
    <Dialog open={props.open} onOpenChange={props.onOpenChange}>
      {props.open && <JobDialogBody {...props} />}
    </Dialog>
  );
}

type Section = 'source' | 'destination' | 'format' | 'schedule' | 'policy';

function JobDialogBody({ onOpenChange, job = null, mode = 'job', onDone }: BackupJobDialogProps) {
  const { t } = useTranslation('backup');
  const formId = useId();
  const qc = useQueryClient();
  const once = mode === 'once';
  const connections = useConnectionsSnapshot();
  const tools = useToolsQuery();
  const profiles = connections.data?.profiles ?? [];

  const [form, setForm] = useState<JobForm>(() =>
    job ? formFromJob(job, undefined) : { ...emptyForm(), scheduled: !once },
  );
  const [section, setSection] = useState<Section>('source');
  const [submitted, setSubmitted] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  const patch = (p: Partial<JobForm>) => setForm((f) => ({ ...f, ...p }));
  const profile: ConnectionProfile | undefined = profiles.find((p) => p.id === form.connectionId);
  const isProduction = profile?.environment === 'production';
  const preview = useSchedulePreview(form.cron, form.timezone);
  const toolInfo = tools.data?.find((x) => x.tool === form.nativeTool);

  const errors: FormErrors = useMemo(() => {
    const e = validateForm(form, {
      once,
      hadPassphrase: !!job?.spec.encrypted,
      databaseOptional: profile?.kind === 'redis',
      toolUsable: tools.data
        ? (tool) => {
            const info = tools.data.find((x) => x.tool === tool);
            return !!info && info.path !== null && info.problem === null;
          }
        : undefined,
    });
    if (!once && form.scheduled && !e.cron && preview.isError) e.cron = 'cronInvalid';
    return e;
  }, [form, once, job, profile, tools.data, preview.isError]);

  const databases = useQuery({
    queryKey: ['backup', 'databases', form.connectionId],
    enabled: !!form.connectionId,
    staleTime: 60_000,
    queryFn: async () => {
      const session = await useSessionsStore.getState().open(form.connectionId);
      const nodes = await ipc.objectChildren(session.sessionId, ROOT_PATH);
      return nodes.filter((n) => n.kind === 'database').map((n) => n.name);
    },
  });

  const selectConnection = (id: string) => {
    const p = profiles.find((x) => x.id === id);
    patch({
      connectionId: id,
      database: p?.database ?? '',
      nativeTool: form.engineKind === 'native' && p ? (NATIVE_TOOLS[p.kind] ?? '') : '',
    });
  };

  const pickDirectory = async () => {
    const dir = await ipc.filePickOpen([], true);
    if (dir) patch({ directory: dir });
  };

  const submit = async () => {
    setSubmitted(true);
    if (saving) return;
    const first = firstErrorSection(errors);
    if (first) {
      setSection(first as Section);
      return;
    }
    setSaving(true);
    setSaveError(null);
    try {
      if (once) {
        await ipc.backupRunManual(buildSpec(form), form.encrypted ? form.passphrase : null, false);
        toast.info(t('once.started'));
      } else {
        const next = buildJob(form, job, crypto.randomUUID(), new Date().toISOString());
        await ipc.backupJobSave(next, passphraseChange(form));
        await qc.invalidateQueries({ queryKey: backupJobsKey });
        void qc.invalidateQueries({ queryKey: connectionsQueryKey });
      }
      onDone?.();
      onOpenChange(false);
    } catch (e) {
      setSaveError(describeError(t, e));
      setSaving(false);
    }
  };

  const sectionHasError = (s: Section) =>
    submitted && Object.keys(errors).some((k) => FIELD_SECTION[k as keyof FormErrors] === s);
  const err = (field: keyof FormErrors): ReactNode =>
    submitted && errors[field] ? (
      <p role="alert" className="text-[11px] text-danger">
        {t(`job.error.${errors[field]}`)}
      </p>
    ) : null;

  const sections: Section[] = once
    ? ['source', 'destination', 'format']
    : ['source', 'destination', 'format', 'schedule', 'policy'];

  const serverSide =
    form.engineKind === 'native' && SERVER_SIDE_TOOLS.includes(form.nativeTool as NativeTool);
  const nativeForKind = profile ? NATIVE_TOOLS[profile.kind] : undefined;
  const example = fileNameExample(form.fileTemplate, {
    connection: profile?.name ?? '',
    database: form.database,
    job: form.name,
  });
  const dbNames = databases.data ?? [];
  const dbOptions =
    form.database && !dbNames.includes(form.database) ? [...dbNames, form.database] : dbNames;
  const level = form.compression === 'none' ? null : COMPRESSION_LEVELS[form.compression];

  return (
    <DialogContent
      title={once ? t('once.title') : job ? t('job.editTitle') : t('job.createTitle')}
      className="w-[min(680px,95vw)]"
      footer={
        <>
          <Button onClick={() => onOpenChange(false)}>{t('actions.cancel')}</Button>
          <Button type="submit" form={formId} variant="primary" disabled={saving}>
            {saving && <Spinner size={12} className="text-accent-foreground" />}
            {once ? t('once.run') : t('actions.save')}
          </Button>
        </>
      }
    >
      <form
        id={formId}
        noValidate
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
        className="flex flex-col gap-3"
      >
        {saveError && (
          <p role="alert" className="rounded-md bg-danger-soft px-2 py-1.5 text-xs text-danger">
            {saveError}
          </p>
        )}
        <Tabs value={section} onValueChange={(v) => setSection(v as Section)}>
          <TabsList>
            {sections.map((s) => (
              <TabsTrigger key={s} value={s} className={sectionHasError(s) ? 'text-danger' : ''}>
                {t(`job.section.${s}`)}
                {sectionHasError(s) && ' *'}
              </TabsTrigger>
            ))}
          </TabsList>

          <TabsContent value="source" className="flex flex-col gap-3 pt-3">
            {!once && (
              <Field label={t('job.name')} htmlFor={`${formId}-name`} error={err('name')}>
                <Input
                  id={`${formId}-name`}
                  value={form.name}
                  aria-invalid={(submitted && !!errors.name) || undefined}
                  onChange={(e) => patch({ name: e.target.value })}
                />
              </Field>
            )}
            <Field label={t('job.connection')} htmlFor={`${formId}-conn`} error={err('connection')}>
              <Select
                id={`${formId}-conn`}
                value={form.connectionId || undefined}
                onValueChange={selectConnection}
                placeholder={t('job.connectionPlaceholder')}
                options={profiles.map((p) => ({ value: p.id, label: p.name }))}
              />
            </Field>
            {isProduction && <p className="text-[11px] text-warning">{t('job.productionNote')}</p>}
            {profile && profile.kind !== 'redis' && (
              <Field label={t('job.database')} htmlFor={`${formId}-db`} error={err('database')}>
                {dbOptions.length > 0 ? (
                  <Select
                    id={`${formId}-db`}
                    value={form.database || undefined}
                    onValueChange={(v) => patch({ database: v })}
                    placeholder={t('job.databasePlaceholder')}
                    options={dbOptions.map((d) => ({ value: d, label: d }))}
                  />
                ) : (
                  <>
                    <Input
                      id={`${formId}-db`}
                      value={form.database}
                      aria-invalid={(submitted && !!errors.database) || undefined}
                      onChange={(e) => patch({ database: e.target.value })}
                    />
                    {databases.isFetching && <Spinner size={12} />}
                    {databases.isError && (
                      <p className="text-[11px] text-muted-foreground">
                        {t('job.databaseListFailed', { error: describeError(t, databases.error) })}
                      </p>
                    )}
                  </>
                )}
              </Field>
            )}
            {!once && (
              <label className="flex items-center gap-2 text-[13px]">
                <Switch
                  checked={form.enabled}
                  onCheckedChange={(v) => patch({ enabled: v })}
                  aria-label={t('job.enabled')}
                />
                {t('job.enabled')}
              </label>
            )}
          </TabsContent>

          <TabsContent value="destination" className="flex flex-col gap-3 pt-3">
            <Field
              label={serverSide ? t('job.serverDirectory') : t('job.directory')}
              htmlFor={`${formId}-dir`}
              error={err('directory')}
            >
              <div className="flex gap-2">
                <Input
                  id={`${formId}-dir`}
                  value={form.directory}
                  aria-invalid={(submitted && !!errors.directory) || undefined}
                  onChange={(e) => patch({ directory: e.target.value })}
                />
                {!serverSide && (
                  <Button onClick={() => void pickDirectory()}>
                    <FolderOpen size={14} aria-hidden /> {t('job.browse')}
                  </Button>
                )}
              </div>
              {serverSide && (
                <p className="text-[11px] text-muted-foreground">{t('job.serverDirectoryHint')}</p>
              )}
            </Field>
            {!serverSide && (
              <Field
                label={t('job.fileTemplate')}
                htmlFor={`${formId}-tpl`}
                error={err('fileTemplate')}
              >
                <Input
                  id={`${formId}-tpl`}
                  value={form.fileTemplate}
                  className="font-mono"
                  aria-invalid={(submitted && !!errors.fileTemplate) || undefined}
                  onChange={(e) => patch({ fileTemplate: e.target.value })}
                />
                <p className="text-[11px] text-muted-foreground">
                  {t('job.fileTemplateHint')}{' '}
                  <code>{'{connection} {database} {date:%Y%m%d-%H%M%S} {job}'}</code>
                </p>
                <p className="text-xs" data-testid="file-example">
                  {t('job.fileExample')}: <code>{example}</code>
                </p>
              </Field>
            )}
          </TabsContent>

          <TabsContent value="format" className="flex flex-col gap-4 pt-3">
            <div className="flex flex-col gap-2">
              <Label>{t('job.engine.label')}</Label>
              <div className="flex gap-2" role="radiogroup" aria-label={t('job.engine.label')}>
                {(['logical', 'native'] as const).map((k) => (
                  <button
                    key={k}
                    type="button"
                    role="radio"
                    aria-checked={form.engineKind === k}
                    disabled={k === 'native' && !!profile && !nativeForKind}
                    onClick={() =>
                      patch({
                        engineKind: k,
                        nativeTool: k === 'native' ? (nativeForKind ?? '') : '',
                      })
                    }
                    className={
                      'rounded-md border px-2 py-1 text-xs disabled:opacity-50 ' +
                      (form.engineKind === k
                        ? 'border-accent bg-accent/10'
                        : 'border-border hover:bg-surface-muted')
                    }
                  >
                    {t(`job.engine.${k}`)}
                  </button>
                ))}
              </div>
              {form.engineKind === 'logical' ? (
                <Field label={t('job.content.label')} htmlFor={`${formId}-content`}>
                  <Select
                    id={`${formId}-content`}
                    value={form.content}
                    onValueChange={(v) => patch({ content: v as ExportContent })}
                    options={(['both', 'schema', 'data'] as const).map((c) => ({
                      value: c,
                      label: t(`job.content.${c}`),
                    }))}
                  />
                </Field>
              ) : (
                <Field
                  label={t('job.nativeTool')}
                  htmlFor={`${formId}-tool`}
                  error={err('nativeTool')}
                >
                  <Select
                    id={`${formId}-tool`}
                    value={form.nativeTool || undefined}
                    onValueChange={(v) => patch({ nativeTool: v as NativeTool })}
                    placeholder={t('job.nativeToolPlaceholder')}
                    options={
                      nativeForKind
                        ? [{ value: nativeForKind, label: t(`tools.name.${nativeForKind}`) }]
                        : []
                    }
                  />
                  {toolInfo &&
                    (toolInfo.problem ? (
                      <p className="text-[11px] text-danger" data-testid="tool-problem">
                        {t('job.toolProblem', { problem: toolInfo.problem })}
                      </p>
                    ) : (
                      <p className="text-[11px] text-muted-foreground">
                        {toolInfo.path}
                        {toolInfo.version ? ` (${toolInfo.version})` : ''}
                      </p>
                    ))}
                </Field>
              )}
            </div>

            <div className="flex flex-col gap-2">
              <div className="grid grid-cols-2 gap-2">
                <Field label={t('job.compression.label')} htmlFor={`${formId}-comp`}>
                  <Select
                    id={`${formId}-comp`}
                    value={form.compression}
                    onValueChange={(v) => {
                      const kind = v as CompressionKind;
                      patch({
                        compression: kind,
                        level: kind === 'none' ? form.level : COMPRESSION_LEVELS[kind].def,
                      });
                    }}
                    options={(['none', 'gzip', 'zstd'] as const).map((c) => ({
                      value: c,
                      label: t(`job.compression.${c}`),
                    }))}
                  />
                </Field>
                {level && (
                  <Field label={t('job.compression.level')} htmlFor={`${formId}-level`}>
                    <Input
                      id={`${formId}-level`}
                      type="number"
                      min={level.min}
                      max={level.max}
                      value={form.level}
                      onChange={(e) => patch({ level: Number(e.target.value) || level.def })}
                    />
                  </Field>
                )}
              </div>
            </div>

            <div className="flex flex-col gap-2">
              {isProduction && !form.encrypted && (
                <div
                  className="flex items-start gap-2 rounded-md border border-warning bg-warning-soft px-3 py-2 text-xs"
                  data-testid="encrypt-suggestion"
                >
                  <ShieldAlert size={16} className="mt-0.5 shrink-0 text-warning" aria-hidden />
                  <div className="flex flex-col items-start gap-1.5">
                    <p className="font-medium">{t('job.encryption.suggestProduction')}</p>
                    <Button size="sm" onClick={() => patch({ encrypted: true })}>
                      {t('job.encryption.enable')}
                    </Button>
                  </div>
                </div>
              )}
              <label className="flex items-center gap-2 text-[13px]">
                <Checkbox
                  checked={form.encrypted}
                  onCheckedChange={(v) => patch({ encrypted: v === true })}
                  aria-label={t('job.encryption.label')}
                />
                {t('job.encryption.label')}
              </label>
              {form.encrypted && (
                <>
                  <p
                    className="flex items-start gap-1.5 rounded-md bg-warning-soft px-2 py-1.5 text-xs text-warning"
                    data-testid="passphrase-warning"
                  >
                    <AlertTriangle size={14} className="mt-0.5 shrink-0" aria-hidden />
                    {t('job.encryption.warning')}
                  </p>
                  <div className="grid grid-cols-2 gap-2">
                    <Field
                      label={t('job.encryption.passphrase')}
                      htmlFor={`${formId}-pp`}
                      error={err('passphrase')}
                    >
                      <Input
                        id={`${formId}-pp`}
                        type="password"
                        autoComplete="new-password"
                        value={form.passphrase}
                        placeholder={job?.spec.encrypted ? t('job.encryption.keepPlaceholder') : ''}
                        onChange={(e) => patch({ passphrase: e.target.value })}
                      />
                    </Field>
                    <Field label={t('job.encryption.repeat')} htmlFor={`${formId}-pp2`}>
                      <Input
                        id={`${formId}-pp2`}
                        type="password"
                        autoComplete="new-password"
                        value={form.passphrase2}
                        onChange={(e) => patch({ passphrase2: e.target.value })}
                      />
                    </Field>
                  </div>
                </>
              )}
            </div>
          </TabsContent>

          {!once && (
            <TabsContent value="schedule" className="flex flex-col gap-3 pt-3">
              <label className="flex items-center gap-2 text-[13px]">
                <Switch
                  checked={form.scheduled}
                  onCheckedChange={(v) => patch({ scheduled: v })}
                  aria-label={t('job.scheduled')}
                />
                {t('job.scheduled')}
              </label>
              {form.scheduled ? (
                <>
                  <CronBuilder
                    cron={form.cron}
                    timezone={form.timezone}
                    onCronChange={(cron) => patch({ cron })}
                    onTimezoneChange={(timezone) => patch({ timezone })}
                    invalid={submitted && !!errors.cron}
                  />
                  <Field label={t('job.missedRun.label')} htmlFor={`${formId}-missed`}>
                    <Select
                      id={`${formId}-missed`}
                      value={form.missedRun}
                      onValueChange={(v) => patch({ missedRun: v as MissedRunPolicy })}
                      options={(['runOnce', 'skip'] as const).map((m) => ({
                        value: m,
                        label: t(`job.missedRun.${m}`),
                      }))}
                    />
                  </Field>
                  {err('cron')}
                </>
              ) : (
                <p className="text-xs text-muted-foreground">{t('job.manualOnly')}</p>
              )}
            </TabsContent>
          )}

          {!once && (
            <TabsContent value="policy" className="flex flex-col gap-4 pt-3">
              <div className="flex flex-col gap-2">
                <p className="text-xs font-medium">{t('job.retention.title')}</p>
                <p className="text-[11px] text-muted-foreground">{t('job.retention.hint')}</p>
                <div className="grid grid-cols-2 gap-2">
                  <Field
                    label={t('job.retention.keepLast')}
                    htmlFor={`${formId}-keep`}
                    error={err('keepLast')}
                  >
                    <Input
                      id={`${formId}-keep`}
                      inputMode="numeric"
                      value={form.keepLast}
                      aria-invalid={(submitted && !!errors.keepLast) || undefined}
                      onChange={(e) => patch({ keepLast: e.target.value })}
                    />
                  </Field>
                  <Field
                    label={t('job.retention.maxAgeDays')}
                    htmlFor={`${formId}-age`}
                    error={err('maxAgeDays')}
                  >
                    <Input
                      id={`${formId}-age`}
                      inputMode="numeric"
                      value={form.maxAgeDays}
                      aria-invalid={(submitted && !!errors.maxAgeDays) || undefined}
                      onChange={(e) => patch({ maxAgeDays: e.target.value })}
                    />
                  </Field>
                </div>
              </div>
              <div className="flex flex-col gap-2">
                <p className="text-xs font-medium">{t('job.notify.title')}</p>
                <label className="flex items-center gap-2 text-[13px]">
                  <Checkbox
                    checked={form.notifyOnSuccess}
                    onCheckedChange={(v) => patch({ notifyOnSuccess: v === true })}
                    aria-label={t('job.notify.onSuccess')}
                  />
                  {t('job.notify.onSuccess')}
                </label>
                <label className="flex items-center gap-2 text-[13px]">
                  <Checkbox
                    checked={form.notifyOnFailure}
                    onCheckedChange={(v) => patch({ notifyOnFailure: v === true })}
                    aria-label={t('job.notify.onFailure')}
                  />
                  {t('job.notify.onFailure')}
                </label>
              </div>
            </TabsContent>
          )}
        </Tabs>
      </form>
    </DialogContent>
  );
}

function Field({
  label,
  htmlFor,
  error,
  children,
}: {
  label: string;
  htmlFor: string;
  error?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="flex flex-col gap-1">
      <Label htmlFor={htmlFor}>{label}</Label>
      {children}
      {error}
    </div>
  );
}

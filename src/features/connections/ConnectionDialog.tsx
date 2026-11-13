import { useQuery, useQueryClient } from '@tanstack/react-query';
import { CheckCircle2, Eye, EyeOff, FolderOpen, XCircle } from 'lucide-react';
import { useId, useMemo, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';
import { Select, type SelectOption } from '@/components/ui/Select';
import { Spinner } from '@/components/ui/Spinner';
import { Switch } from '@/components/ui/Switch';
import { cn } from '@/lib/cn';
import { pickFile } from '@/lib/dialog';
import { ipc, toIpcError } from '@/lib/ipc';
import type { ConnectionProfile, Environment, IpcError, ServerInfo, Uuid } from '@/lib/types';
import { groupPathOptions } from './connectionsTree';
import {
  initialForm,
  profileFromForm,
  saveSecret,
  testSecret,
  validateForm,
  type ConnectionForm,
} from './connectionForm';
import { describeError } from './errors';
import {
  COLOR_SWATCHES,
  DATABASE_FIELD,
  ENVIRONMENT_COLOR,
  ENVIRONMENTS,
  TLS_MODES,
  TLS_VERIFYING,
} from './kindConfig';
import { connectionsQueryKey, useConnectionsSnapshot } from './queries';

export interface ConnectionDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Profile to edit; `undefined` creates a new connection. */
  profile?: ConnectionProfile;
  /** Group preselected for a new connection. */
  groupId?: Uuid | null;
}

const NONE = 'none';

type TestState =
  | { status: 'idle' }
  | { status: 'running' }
  | { status: 'ok'; info: ServerInfo }
  | { status: 'error'; error: IpcError };

/** Create or edit a connection profile (CONN-01..05, CONN-08). */
export function ConnectionDialog({ open, onOpenChange, profile, groupId }: ConnectionDialogProps) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      {open && (
        <ConnectionDialogBody
          key={profile?.id ?? 'new'}
          onClose={() => onOpenChange(false)}
          profile={profile}
          groupId={groupId}
        />
      )}
    </Dialog>
  );
}

function ConnectionDialogBody({
  onClose,
  profile,
  groupId,
}: {
  onClose: () => void;
  profile?: ConnectionProfile;
  groupId?: Uuid | null;
}) {
  const { t } = useTranslation('connections');
  const qc = useQueryClient();
  const formId = useId();
  const isEdit = profile !== undefined;
  const [form, setForm] = useState<ConnectionForm>(() => initialForm(profile, { groupId }));
  const [showPassword, setShowPassword] = useState(false);
  const [submitted, setSubmitted] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [test, setTest] = useState<TestState>({ status: 'idle' });

  const drivers = useQuery({ queryKey: ['drivers'], queryFn: ipc.listDrivers });
  const { data: snapshot } = useConnectionsSnapshot();
  const driver = drivers.data?.find((d) => d.kind === form.kind);
  const errors = useMemo(() => validateForm(form), [form]);
  const showErrors = submitted;

  const update = (patch: Partial<ConnectionForm>) => {
    setForm((f) => ({ ...f, ...patch }));
    setTest({ status: 'idle' });
    setSaveError(null);
  };

  const onKindChange = (kind: string) => {
    const next = drivers.data?.find((d) => d.kind === kind);
    if (!next) return;
    update({
      kind: next.kind,
      ...(form.portTouched ? {} : { port: String(next.defaultPort) }),
    });
  };

  const driverOptions: SelectOption[] = useMemo(() => {
    const list = drivers.data ?? [];
    if (list.length === 0) return [{ value: form.kind, label: form.kind }];
    return list.map((d) => ({
      value: d.kind,
      label:
        d.status.state === 'available'
          ? d.displayName
          : `${d.displayName} (${t('phase', { phase: d.status.phase })})`,
      disabled: d.status.state !== 'available',
    }));
  }, [drivers.data, form.kind, t]);

  const groupOptions: SelectOption[] = useMemo(
    () => [
      { value: NONE, label: t('field.noGroup') },
      ...(snapshot ? groupPathOptions(snapshot).map((g) => ({ value: g.id, label: g.label })) : []),
    ],
    [snapshot, t],
  );

  const environmentOptions: SelectOption[] = [
    { value: NONE, label: t('field.noEnvironment') },
    ...ENVIRONMENTS.map((e) => ({ value: e, label: t(`environment.${e}`) })),
  ];

  const tlsOptions: SelectOption[] = TLS_MODES.map((m) => ({
    value: m,
    label: t(`tlsMode.${m}`),
  }));

  const runTest = async () => {
    // A name is not needed to test; the server address is.
    if (errors.host || errors.port || errors.timeout) {
      setSubmitted(true);
      return;
    }
    setTest({ status: 'running' });
    try {
      const info = await ipc.connectionTest(
        profileFromForm(form, profile, () => crypto.randomUUID()),
        testSecret(form),
      );
      setTest({ status: 'ok', info });
    } catch (e) {
      setTest({ status: 'error', error: toIpcError(e) });
    }
  };

  const submit = async () => {
    setSubmitted(true);
    if (saving || Object.keys(errors).length > 0) return;
    setSaving(true);
    setSaveError(null);
    try {
      await ipc.connectionSave(
        profileFromForm(form, profile, () => crypto.randomUUID()),
        saveSecret(form),
      );
      await qc.invalidateQueries({ queryKey: connectionsQueryKey });
      onClose();
    } catch (e) {
      setSaveError(describeError(t, e));
      setSaving(false);
    }
  };

  const fieldKey = DATABASE_FIELD[form.kind];
  const verifying = TLS_VERIFYING.has(form.tlsMode);
  const defaultColor = form.environment ? ENVIRONMENT_COLOR[form.environment] : undefined;
  const passwordPlaceholder =
    isEdit && profile.hasPassword && form.passwordAction === 'keep'
      ? t('field.passwordKeep')
      : form.passwordAction === 'clear'
        ? t('field.passwordRemoved')
        : '';

  return (
    <DialogContent
      title={isEdit ? t('dialog.editTitle') : t('dialog.newTitle')}
      className="w-[min(580px,94vw)]"
      footer={
        <div className="flex w-full flex-col gap-2">
          <TestResult state={test} />
          <div className="flex items-center gap-2">
            <Button
              onClick={() => void runTest()}
              disabled={test.status === 'running' || saving}
              className="mr-auto"
            >
              {test.status === 'running' && <Spinner size={12} />}
              {t('dialog.test')}
            </Button>
            <Button onClick={onClose}>{t('dialog.cancel')}</Button>
            <Button type="submit" form={formId} variant="primary" disabled={saving}>
              {saving && <Spinner size={12} className="text-accent-foreground" />}
              {t('dialog.save')}
            </Button>
          </div>
        </div>
      }
    >
      <form
        id={formId}
        noValidate
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
        onKeyDown={(e) => {
          // The submit button lives in the dialog footer, outside the form element.
          if (e.key === 'Enter' && e.target instanceof HTMLInputElement) {
            e.preventDefault();
            void submit();
          }
        }}
        className="flex flex-col gap-4"
      >
        {saveError && (
          <p role="alert" className="rounded-md bg-danger-soft px-2 py-1.5 text-xs text-danger">
            {saveError}
          </p>
        )}

        <Section title={t('section.general')}>
          <Field label={t('field.kind')} htmlFor={`${formId}-kind`}>
            <Select
              id={`${formId}-kind`}
              value={form.kind}
              onValueChange={onKindChange}
              options={driverOptions}
            />
          </Field>
          <Field
            label={t('field.name')}
            htmlFor={`${formId}-name`}
            error={showErrors ? errors.name : undefined}
          >
            <Input
              id={`${formId}-name`}
              autoFocus
              value={form.name}
              placeholder={driver?.displayName ?? ''}
              aria-invalid={showErrors && !!errors.name}
              onChange={(e) => update({ name: e.target.value })}
            />
          </Field>
          <div className="grid grid-cols-[1fr_96px] gap-2">
            <Field
              label={t('field.host')}
              htmlFor={`${formId}-host`}
              error={showErrors ? errors.host : undefined}
            >
              <Input
                id={`${formId}-host`}
                value={form.host}
                aria-invalid={showErrors && !!errors.host}
                onChange={(e) => update({ host: e.target.value })}
              />
            </Field>
            <Field
              label={t('field.port')}
              htmlFor={`${formId}-port`}
              error={showErrors ? errors.port : undefined}
            >
              <Input
                id={`${formId}-port`}
                inputMode="numeric"
                value={form.port}
                aria-invalid={showErrors && !!errors.port}
                onChange={(e) => update({ port: e.target.value, portTouched: true })}
              />
            </Field>
          </div>
          <Field label={t(`field.${fieldKey}`)} htmlFor={`${formId}-db`}>
            <Input
              id={`${formId}-db`}
              value={form.database}
              onChange={(e) => update({ database: e.target.value })}
            />
          </Field>
          <Field label={t('field.group')} htmlFor={`${formId}-group`}>
            <Select
              id={`${formId}-group`}
              value={form.groupId || NONE}
              onValueChange={(v) => update({ groupId: v === NONE ? '' : v })}
              options={groupOptions}
            />
          </Field>
          <div className="grid grid-cols-2 gap-2">
            <Field label={t('field.environment')} htmlFor={`${formId}-env`}>
              <Select
                id={`${formId}-env`}
                value={form.environment || NONE}
                onValueChange={(v) => update({ environment: v === NONE ? '' : (v as Environment) })}
                options={environmentOptions}
              />
            </Field>
            <Field label={t('field.color')}>
              <ColorPicker
                value={form.color}
                defaultColor={defaultColor}
                onChange={(color) => update({ color })}
              />
            </Field>
          </div>
          <SwitchRow
            label={t('field.readOnly')}
            hint={t('field.readOnlyHint')}
            checked={form.readOnly}
            onChange={(readOnly) => update({ readOnly })}
          />
        </Section>

        <Section title={t('section.auth')}>
          <Field label={t('field.username')} htmlFor={`${formId}-user`}>
            <Input
              id={`${formId}-user`}
              autoComplete="off"
              value={form.username}
              onChange={(e) => update({ username: e.target.value })}
            />
          </Field>
          <Field label={t('field.password')} htmlFor={`${formId}-pass`}>
            <div className="flex items-center gap-1">
              <Input
                id={`${formId}-pass`}
                type={showPassword ? 'text' : 'password'}
                autoComplete="new-password"
                value={form.password}
                placeholder={passwordPlaceholder}
                onChange={(e) => update({ password: e.target.value, passwordAction: 'keep' })}
              />
              <Button
                size="icon"
                variant="ghost"
                aria-label={showPassword ? t('field.hidePassword') : t('field.showPassword')}
                aria-pressed={showPassword}
                onClick={() => setShowPassword((s) => !s)}
              >
                {showPassword ? <EyeOff size={14} /> : <Eye size={14} />}
              </Button>
            </div>
            {isEdit && profile.hasPassword && !form.password && (
              <button
                type="button"
                onClick={() =>
                  update({ passwordAction: form.passwordAction === 'clear' ? 'keep' : 'clear' })
                }
                className="mt-1 self-start text-xs text-accent underline-offset-2 hover:underline"
              >
                {form.passwordAction === 'clear'
                  ? t('field.undoClearPassword')
                  : t('field.clearPassword')}
              </button>
            )}
          </Field>
          <SwitchRow
            label={t('field.savePassword')}
            hint={t('field.savePasswordHint')}
            checked={form.savePassword}
            onChange={(savePassword) => update({ savePassword })}
          />
        </Section>

        <Section title={t('section.tls')}>
          <Field label={t('tls.mode')} htmlFor={`${formId}-tls`}>
            <Select
              id={`${formId}-tls`}
              value={form.tlsMode}
              onValueChange={(v) => update({ tlsMode: v as ConnectionForm['tlsMode'] })}
              options={tlsOptions}
            />
          </Field>
          {verifying && (
            <>
              <FileField
                id={`${formId}-ca`}
                label={t('tls.caCert')}
                value={form.caCertPath}
                onChange={(caCertPath) => update({ caCertPath })}
              />
              <FileField
                id={`${formId}-cert`}
                label={t('tls.clientCert')}
                value={form.clientCertPath}
                onChange={(clientCertPath) => update({ clientCertPath })}
              />
              <FileField
                id={`${formId}-key`}
                label={t('tls.clientKey')}
                value={form.clientKeyPath}
                onChange={(clientKeyPath) => update({ clientKeyPath })}
              />
            </>
          )}
        </Section>

        <Section title={t('section.advanced')}>
          <Field
            label={t('field.timeout')}
            htmlFor={`${formId}-timeout`}
            error={showErrors ? errors.timeout : undefined}
          >
            <Input
              id={`${formId}-timeout`}
              inputMode="numeric"
              className="w-28"
              value={form.timeoutSec}
              placeholder={t('field.timeoutDefault')}
              aria-invalid={showErrors && !!errors.timeout}
              onChange={(e) => update({ timeoutSec: e.target.value })}
            />
          </Field>
        </Section>
      </form>
    </DialogContent>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-2.5">
      <h3 className="border-b border-border pb-1 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">
        {title}
      </h3>
      {children}
    </section>
  );
}

function Field({
  label,
  htmlFor,
  error,
  children,
}: {
  label: string;
  htmlFor?: string;
  error?: string;
  children: ReactNode;
}) {
  const { t } = useTranslation('connections');
  return (
    <div className="flex min-w-0 flex-col gap-1">
      <Label htmlFor={htmlFor}>{label}</Label>
      {children}
      {error && (
        <p role="alert" className="text-xs text-danger">
          {t(`validation.${error}`)}
        </p>
      )}
    </div>
  );
}

function SwitchRow({
  label,
  hint,
  checked,
  onChange,
}: {
  label: string;
  hint?: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
}) {
  const id = useId();
  return (
    <div className="flex items-start gap-2">
      <Switch id={id} checked={checked} onCheckedChange={onChange} className="mt-0.5" />
      <div className="min-w-0">
        <Label htmlFor={id} className="text-[13px] font-normal text-foreground">
          {label}
        </Label>
        {hint && <p className="text-xs text-muted-foreground">{hint}</p>}
      </div>
    </div>
  );
}

function FileField({
  id,
  label,
  value,
  onChange,
}: {
  id: string;
  label: string;
  value: string;
  onChange: (v: string) => void;
}) {
  const { t } = useTranslation('connections');
  return (
    <Field label={label} htmlFor={id}>
      <div className="flex items-center gap-1">
        <Input id={id} value={value} onChange={(e) => onChange(e.target.value)} />
        <Button
          size="icon"
          variant="outline"
          aria-label={t('tls.browse', { field: label })}
          title={t('tls.browse', { field: label })}
          onClick={() => {
            void pickFile({ title: label }).then((p) => p && onChange(p));
          }}
        >
          <FolderOpen size={14} />
        </Button>
      </div>
    </Field>
  );
}

function ColorPicker({
  value,
  defaultColor,
  onChange,
}: {
  value: string | null;
  defaultColor: string | undefined;
  onChange: (color: string | null) => void;
}) {
  const { t } = useTranslation('connections');
  const swatch = 'size-5 shrink-0 rounded-full border border-border transition-shadow';
  const ring = 'ring-2 ring-foreground ring-offset-1 ring-offset-surface';
  return (
    <div
      role="radiogroup"
      aria-label={t('field.color')}
      className="flex h-7 flex-wrap items-center gap-1.5"
    >
      <button
        type="button"
        role="radio"
        aria-checked={value === null}
        aria-label={t('color.default')}
        title={t('color.default')}
        onClick={() => onChange(null)}
        className={cn(swatch, 'bg-surface-muted', value === null && ring)}
        style={defaultColor ? { backgroundColor: defaultColor } : undefined}
      />
      {COLOR_SWATCHES.map((c) => (
        <button
          key={c.hex}
          type="button"
          role="radio"
          aria-checked={value === c.hex}
          aria-label={t(`color.${c.key}`)}
          title={t(`color.${c.key}`)}
          onClick={() => onChange(c.hex)}
          className={cn(swatch, value === c.hex && ring)}
          style={{ backgroundColor: c.hex }}
        />
      ))}
    </div>
  );
}

function TestResult({ state }: { state: TestState }) {
  const { t } = useTranslation();
  if (state.status === 'idle') return null;
  if (state.status === 'running') {
    return (
      <p role="status" className="flex items-center gap-1.5 text-xs text-muted-foreground">
        <Spinner size={12} />
        {t('connections:test.running')}
      </p>
    );
  }
  if (state.status === 'ok') {
    return (
      <p role="status" className="flex items-start gap-1.5 text-xs text-success">
        <CheckCircle2 size={14} className="mt-px shrink-0" />
        <span className="min-w-0 break-words">
          {t('connections:test.success', {
            product: state.info.product,
            version: state.info.version,
          })}
        </span>
      </p>
    );
  }
  return (
    <p role="alert" className="flex items-start gap-1.5 text-xs text-danger">
      <XCircle size={14} className="mt-px shrink-0" />
      <span className="min-w-0 select-text break-words">{describeError(t, state.error)}</span>
    </p>
  );
}

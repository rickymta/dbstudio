import { useQuery, useQueryClient } from '@tanstack/react-query';
import { CheckCircle2, Eye, EyeOff, FolderOpen, TriangleAlert, XCircle } from 'lucide-react';
import { useEffect, useId, useMemo, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';
import { Select, type SelectOption } from '@/components/ui/Select';
import { Spinner } from '@/components/ui/Spinner';
import { Switch } from '@/components/ui/Switch';
import { toast } from '@/components/ui/toastStore';
import { cn } from '@/lib/cn';
import { pickFile } from '@/lib/dialog';
import { ipc, toIpcError } from '@/lib/ipc';
import type {
  ConnectionProfile,
  ConnectionsSnapshot,
  Environment,
  IpcError,
  ServerInfo,
  Uuid,
} from '@/lib/types';
import { groupPathOptions } from './connectionsTree';
import {
  autoName,
  defaultUsername,
  initialForm,
  profileFromForm,
  saveSecret,
  testSecret,
  validateForm,
  type ConnectionForm,
} from './connectionForm';
import { describeError } from './errors';
import { SlowConnectHint } from './SlowConnectHint';
import {
  COLOR_SWATCHES,
  databaseFieldFor,
  ENVIRONMENT_COLOR,
  ENVIRONMENTS,
  TLS_MODES,
  TLS_VERIFYING,
} from './kindConfig';
import { needsTlsWarning } from './tlsWarning';
import { OptionsEditor } from '@/features/document/OptionsEditor';
import { KindOptions } from './KindOptions';
import { optionsForKind } from './kindOptionFields';
import { UriPaste } from '@/features/document/UriPaste';
import { connectionsQueryKey, useConnectionsSnapshot } from './queries';
import { useHostKeyPrompt } from './useHostKeyPrompt';
import { SshSection } from './SshSection';
import {
  initialSshForm,
  sshSecretChange,
  validateSsh,
  withSsh,
  type SshFormState,
} from './sshForm';

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
  // Bumped on every rejected submit so the first invalid field gets focus (and scrolls into view).
  const [focusTick, setFocusTick] = useState(0);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [test, setTest] = useState<TestState>({ status: 'idle' });
  const [ssh, setSsh] = useState<SshFormState>(() => initialSshForm(profile));
  const sshErrors = useMemo(() => validateSsh(ssh), [ssh]);
  const { run: runWithHostKey, dialog: hostKeyDialog } = useHostKeyPrompt();

  const drivers = useQuery({ queryKey: ['drivers'], queryFn: ipc.listDrivers });
  const { data: snapshot } = useConnectionsSnapshot();
  const errors = useMemo(() => validateForm(form), [form]);
  // A blank name is saved as `<kind> N` (the first number free among the other connections).
  const suggestedName = useMemo(
    () =>
      autoName(
        form.kind,
        (snapshot?.profiles ?? []).filter((p) => p.id !== profile?.id).map((p) => p.name),
      ),
    [form.kind, snapshot, profile?.id],
  );
  const named = (f: ConnectionForm): ConnectionForm =>
    f.name.trim() ? f : { ...f, name: suggestedName };
  const showErrors = submitted;

  useEffect(() => {
    if (focusTick === 0) return;
    const field = document
      .getElementById(formId)
      ?.querySelector<HTMLElement>('[aria-invalid="true"]');
    if (!field) return;
    field.scrollIntoView?.({ block: 'center' });
    field.focus();
  }, [focusTick, formId]);

  const update = (patch: Partial<ConnectionForm>) => {
    setForm((f) => ({ ...f, ...patch }));
    setTest({ status: 'idle' });
    setSaveError(null);
  };

  const updateSsh = (patch: Partial<SshFormState>) => {
    setSsh((s) => ({ ...s, ...patch }));
    setTest({ status: 'idle' });
    setSaveError(null);
  };

  // A blank name stays blank: it is saved under the suggested `<kind> N` name.
  const onUriParsed = (patch: Partial<ConnectionForm>) => update(patch);

  const onKindChange = (kind: string) => {
    const next = drivers.data?.find((d) => d.kind === kind);
    if (!next) return;
    update({
      kind: next.kind,
      options: optionsForKind(form.options, next.kind),
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
          : d.status.state === 'planned'
            ? `${d.displayName} (${t('phase', { phase: d.status.phase })})`
            : `${d.displayName} (${t('driverUnavailable')})`,
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

  const authOptions: SelectOption[] = [
    { value: 'password', label: t('authMode.password') },
    { value: 'none', label: t('authMode.none') },
  ];

  const tlsOptions: SelectOption[] = TLS_MODES.map((m) => ({
    value: m,
    label: t(`tlsMode.${m}`),
  }));

  const runTest = async () => {
    // A name is not needed to test; the server address is.
    if (errors.host || errors.port || errors.timeout || Object.keys(sshErrors).length > 0) {
      setSubmitted(true);
      setFocusTick((n) => n + 1);
      return;
    }
    setTest({ status: 'running' });
    try {
      const testProfile = withSsh(
        profileFromForm(named(form), profile, () => crypto.randomUUID()),
        ssh,
      );
      // The SSH password typed in the dialog is used for this test even before it is saved.
      const sshSecret = sshSecretChange(ssh, profile);
      const info = await runWithHostKey(() =>
        sshSecret
          ? ipc.connectionTest(testProfile, testSecret(form), sshSecret)
          : ipc.connectionTest(testProfile, testSecret(form)),
      );
      setTest({ status: 'ok', info });
    } catch (e) {
      setTest({ status: 'error', error: toIpcError(e) });
    }
  };

  const submit = async () => {
    setSubmitted(true);
    if (saving) return;
    if (Object.keys(errors).length > 0 || Object.keys(sshErrors).length > 0) {
      setFocusTick((n) => n + 1);
      return;
    }
    setSaving(true);
    setSaveError(null);
    const built = withSsh(
      profileFromForm(named(form), profile, () => crypto.randomUUID()),
      ssh,
    );
    const sshSecret = sshSecretChange(ssh, profile);
    try {
      await (sshSecret
        ? ipc.connectionSave(built, saveSecret(form), sshSecret)
        : ipc.connectionSave(built, saveSecret(form)));
      await qc.invalidateQueries({ queryKey: connectionsQueryKey });
      onClose();
    } catch (e) {
      const error = toIpcError(e);
      // The backend may persist the profile and still fail (e.g. keychain error), so always reload.
      await qc.invalidateQueries({ queryKey: connectionsQueryKey });
      const saved = qc
        .getQueryData<ConnectionsSnapshot>(connectionsQueryKey)
        ?.profiles.find((p) => p.id === built.id);
      const persisted = saved !== undefined && (!profile || saved.updatedAt !== profile.updatedAt);
      if (error.kind === 'storage' && persisted) {
        toast.error(describeError(t, error));
        onClose();
        return;
      }
      setSaveError(describeError(t, error));
      setSaving(false);
    }
  };

  // A typed password is not read from the keychain, but a stored one (or saving one) is.
  const usesKeychain =
    form.authMode === 'password' &&
    form.savePassword &&
    (form.password !== '' || (profile?.hasPassword ?? false));
  const fieldKey = databaseFieldFor(form.kind);
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
          <SlowConnectHint active={test.status === 'running' || saving} keychain={usesKeychain} />
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
          {!isEdit && <UriPaste id={`${formId}-uri`} onParsed={onUriParsed} />}
          <Field label={t('field.kind')} htmlFor={`${formId}-kind`}>
            <Select
              id={`${formId}-kind`}
              value={form.kind}
              onValueChange={onKindChange}
              options={driverOptions}
            />
          </Field>
          <Field label={t('field.name')} htmlFor={`${formId}-name`}>
            <Input
              id={`${formId}-name`}
              autoFocus
              value={form.name}
              placeholder={suggestedName}
              title={form.name.trim() ? undefined : t('field.nameAuto', { name: suggestedName })}
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
          <Field label={t('field.authMode')} htmlFor={`${formId}-authmode`}>
            <Select
              id={`${formId}-authmode`}
              value={form.authMode}
              onValueChange={(v) => update({ authMode: v as ConnectionForm['authMode'] })}
              options={authOptions}
            />
            {form.authMode === 'none' && (
              <p className="text-xs text-muted-foreground">{t('field.authModeNoneHint')}</p>
            )}
          </Field>
          <Field
            label={
              form.authMode === 'none'
                ? `${t('field.username')} (${t('field.optional')})`
                : t('field.username')
            }
            htmlFor={`${formId}-user`}
          >
            <Input
              id={`${formId}-user`}
              autoComplete="off"
              value={form.username}
              placeholder={
                form.authMode === 'none' && defaultUsername(form.kind)
                  ? t('field.usernameDefault', { name: defaultUsername(form.kind) })
                  : ''
              }
              onChange={(e) => update({ username: e.target.value })}
            />
          </Field>
          {form.authMode === 'password' && (
            <>
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
            </>
          )}
        </Section>

        <Section title={t('section.tls')}>
          <Field label={t('tls.mode')} htmlFor={`${formId}-tls`}>
            <Select
              id={`${formId}-tls`}
              value={form.tlsMode}
              onValueChange={(v) => update({ tlsMode: v as ConnectionForm['tlsMode'] })}
              options={tlsOptions}
            />
            <p className="text-xs text-muted-foreground">{t(`tlsHint.${form.tlsMode}`)}</p>
            {needsTlsWarning(form.tlsMode, form.host) && (
              <p
                role="note"
                data-testid="tls-warning"
                className={cn(
                  'flex items-start gap-1.5 rounded-md px-2 py-1.5 text-xs',
                  form.environment === 'production'
                    ? 'bg-danger-soft text-danger'
                    : 'bg-warning-soft text-warning',
                )}
              >
                <TriangleAlert size={14} className="mt-px shrink-0" aria-hidden />
                <span className="min-w-0">{t(`tlsWarning.${form.tlsMode}`)}</span>
              </p>
            )}
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

        <SshSection
          value={ssh}
          onChange={updateSsh}
          errors={showErrors ? sshErrors : {}}
          profile={profile}
        />

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
          <KindOptions
            kind={form.kind}
            value={form.options}
            onChange={(options) => update({ options })}
          />
          {form.kind === 'mongodb' && (
            <OptionsEditor value={form.options} onChange={(options) => update({ options })} />
          )}
        </Section>
      </form>
      {hostKeyDialog}
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

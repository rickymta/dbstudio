import { Eye, EyeOff, FolderOpen } from 'lucide-react';
import { useId, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';
import { Select, type SelectOption } from '@/components/ui/Select';
import { Switch } from '@/components/ui/Switch';
import { pickFile } from '@/lib/dialog';
import { hadSshSecret, needsSshSecret, type SshErrors, type SshFormState } from './sshForm';
import type { ConnectionProfile } from '@/lib/types';

export interface SshSectionProps {
  value: SshFormState;
  onChange: (patch: Partial<SshFormState>) => void;
  /** Validation errors (already filtered: pass `{}` to hide them). */
  errors: SshErrors;
  /** Profile being edited, to know whether a secret is stored. */
  profile?: ConnectionProfile;
}

const METHODS = ['password', 'privateKey', 'agent'] as const;

/** SSH tunnel settings of the connection dialog (CONN-06). */
export function SshSection({ value, onChange, errors, profile }: SshSectionProps) {
  const { t } = useTranslation('ssh');
  const id = useId();
  const [showSecret, setShowSecret] = useState(false);
  const stored = hadSshSecret(profile);
  const methodOptions: SelectOption[] = METHODS.map((m) => ({
    value: m,
    label: t(`auth.${m}`),
  }));
  const secretLabel = value.method === 'password' ? t('password') : t('passphrase');
  const secretPlaceholder =
    stored && !value.secret
      ? value.secretAction === 'clear'
        ? t('secretRemoved')
        : t('secretKeep')
      : '';

  return (
    <section className="flex flex-col gap-2.5" aria-label={t('section')}>
      <h3 className="border-b border-border pb-1 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">
        {t('section')}
      </h3>
      <div className="flex items-start gap-2">
        <Switch
          id={`${id}-enable`}
          checked={value.enabled}
          onCheckedChange={(enabled) => onChange({ enabled })}
          className="mt-0.5"
        />
        <div className="min-w-0">
          <Label htmlFor={`${id}-enable`} className="text-[13px] font-normal text-foreground">
            {t('enable')}
          </Label>
          <p className="text-xs text-muted-foreground">{t('enableHint')}</p>
        </div>
      </div>

      {value.enabled && (
        <>
          <div className="grid grid-cols-[1fr_96px] gap-2">
            <Field label={t('host')} htmlFor={`${id}-host`} error={errors.host}>
              <Input
                id={`${id}-host`}
                value={value.host}
                autoComplete="off"
                aria-invalid={!!errors.host}
                onChange={(e) => onChange({ host: e.target.value })}
              />
            </Field>
            <Field label={t('port')} htmlFor={`${id}-port`} error={errors.port}>
              <Input
                id={`${id}-port`}
                inputMode="numeric"
                value={value.port}
                aria-invalid={!!errors.port}
                onChange={(e) => onChange({ port: e.target.value })}
              />
            </Field>
          </div>
          <Field label={t('username')} htmlFor={`${id}-user`} error={errors.username}>
            <Input
              id={`${id}-user`}
              autoComplete="off"
              value={value.username}
              aria-invalid={!!errors.username}
              onChange={(e) => onChange({ username: e.target.value })}
            />
          </Field>
          <Field label={t('auth.label')} htmlFor={`${id}-method`}>
            <Select
              id={`${id}-method`}
              value={value.method}
              onValueChange={(method) =>
                onChange({
                  method: method as SshFormState['method'],
                  secret: '',
                  secretAction: 'keep',
                })
              }
              options={methodOptions}
            />
          </Field>

          {value.method === 'privateKey' && (
            <>
              <Field label={t('keyPath')} htmlFor={`${id}-key`} error={errors.keyPath}>
                <div className="flex items-center gap-1">
                  <Input
                    id={`${id}-key`}
                    value={value.keyPath}
                    aria-invalid={!!errors.keyPath}
                    onChange={(e) => onChange({ keyPath: e.target.value })}
                  />
                  <Button
                    size="icon"
                    variant="outline"
                    aria-label={t('browseKey')}
                    title={t('browseKey')}
                    onClick={() => {
                      void pickFile({ title: t('keyPath') }).then(
                        (p) => p && onChange({ keyPath: p }),
                      );
                    }}
                  >
                    <FolderOpen size={14} />
                  </Button>
                </div>
              </Field>
              <div className="flex items-start gap-2">
                <Switch
                  id={`${id}-haspass`}
                  checked={value.hasPassphrase}
                  onCheckedChange={(hasPassphrase) =>
                    onChange({ hasPassphrase, secret: '', secretAction: 'keep' })
                  }
                  className="mt-0.5"
                />
                <Label
                  htmlFor={`${id}-haspass`}
                  className="text-[13px] font-normal text-foreground"
                >
                  {t('hasPassphrase')}
                </Label>
              </div>
            </>
          )}

          {needsSshSecret(value) && (
            <Field label={secretLabel} htmlFor={`${id}-secret`}>
              <div className="flex items-center gap-1">
                <Input
                  id={`${id}-secret`}
                  type={showSecret ? 'text' : 'password'}
                  autoComplete="new-password"
                  value={value.secret}
                  placeholder={secretPlaceholder}
                  onChange={(e) => onChange({ secret: e.target.value, secretAction: 'keep' })}
                />
                <Button
                  size="icon"
                  variant="ghost"
                  aria-label={showSecret ? t('hideSecret') : t('showSecret')}
                  aria-pressed={showSecret}
                  onClick={() => setShowSecret((s) => !s)}
                >
                  {showSecret ? <EyeOff size={14} /> : <Eye size={14} />}
                </Button>
              </div>
              {stored && !value.secret && (
                <button
                  type="button"
                  onClick={() =>
                    onChange({ secretAction: value.secretAction === 'clear' ? 'keep' : 'clear' })
                  }
                  className="mt-1 self-start text-xs text-accent underline-offset-2 hover:underline"
                >
                  {value.secretAction === 'clear' ? t('undoClearSecret') : t('clearSecret')}
                </button>
              )}
            </Field>
          )}
          {value.method === 'agent' && (
            <p className="text-xs text-muted-foreground">{t('agentHint')}</p>
          )}

          <Field label={t('keepalive')} htmlFor={`${id}-keepalive`} error={errors.keepalive}>
            <Input
              id={`${id}-keepalive`}
              inputMode="numeric"
              className="w-28"
              value={value.keepaliveSec}
              aria-invalid={!!errors.keepalive}
              onChange={(e) => onChange({ keepaliveSec: e.target.value })}
            />
            <p className="text-xs text-muted-foreground">{t('keepaliveHint')}</p>
          </Field>
          {needsSshSecret(value) && !stored && (
            <p className="text-xs text-muted-foreground">{t('testNote')}</p>
          )}
        </>
      )}
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
  const { t } = useTranslation('ssh');
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

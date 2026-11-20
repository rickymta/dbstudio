import { useQuery } from '@tanstack/react-query';
import { CheckCircle2, CircleAlert, Copy, Send } from 'lucide-react';
import { useId, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Checkbox } from '@/components/ui/Checkbox';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';
import { Select } from '@/components/ui/Select';
import { Spinner } from '@/components/ui/Spinner';
import { Textarea } from '@/components/ui/Textarea';
import { toast } from '@/components/ui/toastStore';
import { describeError } from '@/features/connections/errors';
import { ipc } from '@/lib/ipc';
import type { SupportTopic } from '@/lib/types';
import {
  LIMITS,
  TOPICS,
  emptyForm,
  rememberContact,
  toRequest,
  validate,
  type FieldName,
  type FormErrors,
  type SupportForm,
} from './support';

type Status =
  { kind: 'idle' } | { kind: 'sending' } | { kind: 'sent' } | { kind: 'error'; message: string };

export interface HelpDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/**
 * Help & Support: the running version, and a form sent to the developer as an email. An unsent
 * draft survives closing the dialog; it is cleared after a successful send.
 */
export function HelpDialog({ open, onOpenChange }: HelpDialogProps) {
  const { t } = useTranslation('help');
  const id = useId();
  const [form, setForm] = useState<SupportForm>(emptyForm);
  const [errors, setErrors] = useState<FormErrors>({});
  const [status, setStatus] = useState<Status>({ kind: 'idle' });
  const { data: info } = useQuery({ queryKey: ['appInfo'], queryFn: ipc.appInfo });
  const sending = status.kind === 'sending';

  const versionLine = info
    ? `${info.name} ${info.version}${info.os !== 'browser' ? ` · ${info.os} (${info.arch})` : ''}`
    : t('common:app.name');

  const set = <K extends keyof SupportForm>(key: K, value: SupportForm[K]) => {
    setForm((f) => ({ ...f, [key]: value }));
    if (key in errors) setErrors((e) => ({ ...e, [key]: undefined }));
    if (status.kind === 'error') setStatus({ kind: 'idle' });
  };

  const startOver = () => {
    setForm(emptyForm());
    setStatus({ kind: 'idle' });
  };

  const close = (next: boolean) => {
    if (next || sending) return;
    if (status.kind === 'sent') startOver();
    setErrors({});
    onOpenChange(false);
  };

  const submit = async () => {
    if (sending) return;
    const found = validate(form);
    setErrors(found);
    const first = (Object.keys(found) as FieldName[])[0];
    if (first) {
      document.getElementById(`${id}-${first}`)?.focus();
      return;
    }
    setStatus({ kind: 'sending' });
    try {
      const request = toRequest(form);
      await ipc.supportSend(request);
      rememberContact(request.name, request.email);
      setStatus({ kind: 'sent' });
    } catch (e) {
      setStatus({ kind: 'error', message: describeError(t, e) });
    }
  };

  const copyVersion = () => {
    void navigator.clipboard
      ?.writeText(versionLine)
      .then(() => toast.info(t('versionCopied')))
      .catch(() => undefined);
  };

  const field = (name: FieldName) => ({
    id: `${id}-${name}`,
    'aria-invalid': !!errors[name] || undefined,
    disabled: sending,
  });

  return (
    <Dialog open={open} onOpenChange={close}>
      {open && (
        <DialogContent
          title={t('title')}
          description={t('description')}
          className="w-[min(600px,94vw)]"
          footer={
            status.kind === 'sent' ? (
              <>
                <Button onClick={startOver}>{t('sendAnother')}</Button>
                <Button variant="primary" onClick={() => close(false)}>
                  {t('common:actions.close')}
                </Button>
              </>
            ) : (
              <>
                <Button onClick={() => close(false)} disabled={sending}>
                  {t('common:actions.cancel')}
                </Button>
                <Button type="submit" form={id} variant="primary" disabled={sending}>
                  {sending ? (
                    <Spinner size={12} className="text-accent-foreground" />
                  ) : (
                    <Send size={13} />
                  )}
                  {sending ? t('sending') : status.kind === 'error' ? t('tryAgain') : t('submit')}
                </Button>
              </>
            )
          }
        >
          <div
            className="mb-3 flex items-center gap-2 rounded-md border border-border bg-surface-muted px-3 py-2"
            data-testid="help-version"
          >
            <div className="min-w-0 flex-1">
              <div className="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">
                {t('version')}
              </div>
              <code className="select-text text-xs">{versionLine}</code>
            </div>
            <Button size="sm" variant="ghost" onClick={copyVersion}>
              <Copy size={12} />
              {t('copyVersion')}
            </Button>
          </div>

          {status.kind === 'sent' ? (
            <div className="flex flex-col items-center gap-2 py-6 text-center" role="status">
              <CheckCircle2 size={36} className="text-success" aria-hidden />
              <div className="text-sm font-semibold">{t('sent')}</div>
              <p className="max-w-sm text-xs text-muted-foreground">
                {t('sentHint', { email: form.email.trim() })}
              </p>
            </div>
          ) : (
            <form
              id={id}
              noValidate
              className="flex flex-col gap-3"
              onSubmit={(e) => {
                e.preventDefault();
                void submit();
              }}
            >
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                <Field label={t('name')} htmlFor={`${id}-name`} error={errors.name}>
                  <Input
                    {...field('name')}
                    maxLength={LIMITS.name}
                    autoComplete="name"
                    value={form.name}
                    onChange={(e) => set('name', e.target.value)}
                  />
                </Field>
                <Field label={t('email')} htmlFor={`${id}-email`} error={errors.email}>
                  <Input
                    {...field('email')}
                    type="email"
                    autoComplete="email"
                    spellCheck={false}
                    value={form.email}
                    onChange={(e) => set('email', e.target.value)}
                  />
                </Field>
              </div>
              <Field label={t('topic')} htmlFor={`${id}-topic`} error={errors.topic}>
                <Select
                  id={`${id}-topic`}
                  disabled={sending}
                  value={form.topic || undefined}
                  placeholder={t('topicPlaceholder')}
                  onValueChange={(v) => set('topic', v as SupportTopic)}
                  options={TOPICS.map((topic) => ({ value: topic, label: t(`topics.${topic}`) }))}
                />
              </Field>
              <Field label={t('subject')} htmlFor={`${id}-subject`} error={errors.subject}>
                <Input
                  {...field('subject')}
                  maxLength={LIMITS.subject}
                  value={form.subject}
                  onChange={(e) => set('subject', e.target.value)}
                />
              </Field>
              <Field label={t('message')} htmlFor={`${id}-message`} error={errors.message}>
                <Textarea
                  {...field('message')}
                  rows={7}
                  maxLength={LIMITS.message}
                  className="font-sans"
                  placeholder={t('messagePlaceholder')}
                  value={form.message}
                  onChange={(e) => set('message', e.target.value)}
                />
              </Field>
              <div className="flex items-start gap-2">
                <Checkbox
                  id={`${id}-system`}
                  className="mt-0.5"
                  checked={form.includeSystem}
                  disabled={sending}
                  onCheckedChange={(v) => set('includeSystem', v === true)}
                />
                <div className="min-w-0">
                  <Label htmlFor={`${id}-system`} className="font-normal">
                    {t('includeSystem')}
                  </Label>
                  <p className="text-xs text-muted-foreground">{t('privacy')}</p>
                </div>
              </div>
              {status.kind === 'error' && (
                <div
                  role="alert"
                  className="flex items-start gap-2 rounded-md bg-danger-soft px-3 py-2 text-xs text-danger"
                >
                  <CircleAlert size={14} className="mt-0.5 shrink-0" aria-hidden />
                  <div className="min-w-0">
                    <div className="font-medium">{t('sendFailed')}</div>
                    <div className="break-words select-text">{status.message}</div>
                  </div>
                </div>
              )}
            </form>
          )}
        </DialogContent>
      )}
    </Dialog>
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
  error?: string;
  children: ReactNode;
}) {
  const { t } = useTranslation('help');
  return (
    <div className="flex min-w-0 flex-col gap-1">
      <Label htmlFor={htmlFor}>{label}</Label>
      {children}
      {error && (
        <p role="alert" className="text-xs text-danger">
          {t(`error.${error}`)}
        </p>
      )}
    </div>
  );
}

import { useQuery } from '@tanstack/react-query';
import { Copy, FolderOpen } from 'lucide-react';
import { useId, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';
import { Select } from '@/components/ui/Select';
import { Spinner } from '@/components/ui/Spinner';
import { toast } from '@/components/ui/toastStore';
import { describeError } from '@/features/connections/errors';
import { ipc } from '@/lib/ipc';
import { setLanguage, type Language } from '@/lib/i18n';
import type { AppSettings, LogLevel, TimestampDisplay } from '@/lib/types';
import { THEME_MODES, useThemeStore, type ThemeMode } from '@/stores/theme';
import { CELLS_RANGE, parseBounded, ROWS_RANGE, useAppSettings, useSaveSettings } from './queries';

const LOG_LEVELS: LogLevel[] = ['error', 'warn', 'info', 'debug', 'trace'];
const TIMESTAMPS: TimestampDisplay[] = ['utc', 'local'];

export interface SettingsDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/** Application settings (APP-05, APP-07, NFR-PERF-03). */
export function SettingsDialog({ open, onOpenChange }: SettingsDialogProps) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      {open && <SettingsBody onClose={() => onOpenChange(false)} />}
    </Dialog>
  );
}

function SettingsBody({ onClose }: { onClose: () => void }) {
  const { t, i18n } = useTranslation('settings');
  const id = useId();
  const current = useAppSettings();
  const save = useSaveSettings();
  const mode = useThemeStore((s) => s.mode);
  const setMode = useThemeStore((s) => s.setMode);
  const [logLevel, setLogLevel] = useState<LogLevel>(current.logLevel);
  const [timestamp, setTimestamp] = useState<TimestampDisplay>(current.timestampDisplay);
  const [rows, setRows] = useState(String(current.gridMaxRows));
  const [cells, setCells] = useState(String(current.gridMaxCells));
  const [submitted, setSubmitted] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const logDir = useQuery({ queryKey: ['logDir'], queryFn: ipc.logDir, staleTime: Infinity });

  const rowsValue = parseBounded(rows, ROWS_RANGE);
  const cellsValue = parseBounded(cells, CELLS_RANGE);
  const language: Language = i18n.language.startsWith('vi') ? 'vi' : 'en';

  const submit = async () => {
    setSubmitted(true);
    if (saving || rowsValue === null || cellsValue === null) return;
    setSaving(true);
    setError(null);
    const next: AppSettings = {
      ...current,
      logLevel,
      gridMaxRows: rowsValue,
      gridMaxCells: cellsValue,
      timestampDisplay: timestamp,
    };
    try {
      await save(next);
      onClose();
    } catch (e) {
      setError(describeError(t, e));
      setSaving(false);
    }
  };

  const copyPath = () => {
    if (!logDir.data) return;
    void navigator.clipboard
      ?.writeText(logDir.data)
      .then(() => toast.info(t('copied')))
      .catch(() => undefined);
  };

  return (
    <DialogContent
      title={t('title')}
      className="w-[min(520px,94vw)]"
      footer={
        <>
          <Button onClick={onClose}>{t('cancel')}</Button>
          <Button type="submit" form={id} variant="primary" disabled={saving}>
            {saving && <Spinner size={12} className="text-accent-foreground" />}
            {t('save')}
          </Button>
        </>
      }
    >
      <form
        id={id}
        noValidate
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
        onKeyDown={(e) => {
          if (e.key === 'Enter' && e.target instanceof HTMLInputElement) {
            e.preventDefault();
            void submit();
          }
        }}
      >
        {error && (
          <p role="alert" className="rounded-md bg-danger-soft px-2 py-1.5 text-xs text-danger">
            {error}
          </p>
        )}

        <Section title={t('section.appearance')}>
          <div className="grid grid-cols-2 gap-2">
            <Field label={t('language')} htmlFor={`${id}-lang`}>
              <Select
                id={`${id}-lang`}
                value={language}
                onValueChange={(v) => setLanguage(v as Language)}
                options={(['vi', 'en'] as const).map((l) => ({
                  value: l,
                  label: t(`common:language.${l}`),
                }))}
              />
            </Field>
            <Field label={t('theme')} htmlFor={`${id}-theme`}>
              <Select
                id={`${id}-theme`}
                value={mode}
                onValueChange={(v) => setMode(v as ThemeMode)}
                options={THEME_MODES.map((m) => ({ value: m, label: t(`common:theme.${m}`) }))}
              />
            </Field>
          </div>
        </Section>

        <Section title={t('section.data')}>
          <div className="grid grid-cols-2 gap-2">
            <Field
              label={t('gridMaxRows')}
              htmlFor={`${id}-rows`}
              error={submitted && rowsValue === null ? t('validation.rows') : undefined}
            >
              <Input
                id={`${id}-rows`}
                inputMode="numeric"
                value={rows}
                aria-invalid={submitted && rowsValue === null}
                onChange={(e) => setRows(e.target.value)}
              />
            </Field>
            <Field
              label={t('gridMaxCells')}
              htmlFor={`${id}-cells`}
              error={submitted && cellsValue === null ? t('validation.cells') : undefined}
            >
              <Input
                id={`${id}-cells`}
                inputMode="numeric"
                value={cells}
                aria-invalid={submitted && cellsValue === null}
                onChange={(e) => setCells(e.target.value)}
              />
            </Field>
          </div>
          <p className="text-xs text-muted-foreground">{t('gridHint')}</p>
          <Field label={t('timestampDisplay')} htmlFor={`${id}-ts`}>
            <Select
              id={`${id}-ts`}
              value={timestamp}
              onValueChange={(v) => setTimestamp(v as TimestampDisplay)}
              options={TIMESTAMPS.map((v) => ({ value: v, label: t(`timestamp.${v}`) }))}
            />
          </Field>
        </Section>

        <Section title={t('section.logging')}>
          <Field label={t('logLevel')} htmlFor={`${id}-level`}>
            <Select
              id={`${id}-level`}
              value={logLevel}
              onValueChange={(v) => setLogLevel(v as LogLevel)}
              options={LOG_LEVELS.map((l) => ({ value: l, label: t(`level.${l}`) }))}
            />
          </Field>
          <Field label={t('logDir')}>
            <div className="flex items-center gap-1">
              <code
                data-testid="log-dir"
                title={logDir.data}
                className="min-w-0 flex-1 select-text truncate rounded-md border border-border bg-surface-muted px-2 py-1 text-xs"
              >
                {logDir.data ?? t('logDirLoading')}
              </code>
              <Button
                size="icon"
                variant="outline"
                aria-label={t('openLogDir')}
                title={t('openLogDir')}
                onClick={() => {
                  void ipc.openLogDir().catch((e: unknown) => toast.error(describeError(t, e)));
                }}
              >
                <FolderOpen size={14} />
              </Button>
              <Button
                size="icon"
                variant="outline"
                aria-label={t('copyPath')}
                title={t('copyPath')}
                disabled={!logDir.data}
                onClick={copyPath}
              >
                <Copy size={14} />
              </Button>
            </div>
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
  return (
    <div className="flex min-w-0 flex-col gap-1">
      <Label htmlFor={htmlFor}>{label}</Label>
      {children}
      {error && (
        <p role="alert" className="text-xs text-danger">
          {error}
        </p>
      )}
    </div>
  );
}

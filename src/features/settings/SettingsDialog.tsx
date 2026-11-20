import { AppearanceFields } from './AppearanceFields';
import { useAppearanceStore } from '@/stores/appearance';
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
import { Switch } from '@/components/ui/Switch';
import { toast } from '@/components/ui/toastStore';
import { BackgroundSettings } from '@/features/backup/BackgroundSettings';
import { describeError } from '@/features/connections/errors';
import { ipc } from '@/lib/ipc';
import { setLanguage, type Language } from '@/lib/i18n';
import type { AppSettings, LogLevel, TimestampDisplay } from '@/lib/types';
import { THEME_MODES, useThemeStore, type ThemeMode } from '@/stores/theme';
import {
  CELLS_RANGE,
  NUMBER_FIELDS,
  parseBounded,
  parseNumberField,
  ROWS_RANGE,
  useAppSettings,
  useSaveSettings,
  type NumberFieldKey,
} from './queries';

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
  const [appearance, setAppearance] = useState(useAppearanceStore.getState().appearance);
  const save = useSaveSettings();
  const mode = useThemeStore((s) => s.mode);
  const setMode = useThemeStore((s) => s.setMode);
  const [logLevel, setLogLevel] = useState<LogLevel>(current.logLevel);
  const [timestamp, setTimestamp] = useState<TimestampDisplay>(current.timestampDisplay);
  const [advisorMetadata, setAdvisorMetadata] = useState(current.planAdvisorMetadata);
  const [rows, setRows] = useState(String(current.gridMaxRows));
  const [cells, setCells] = useState(String(current.gridMaxCells));
  const [numbers, setNumbers] = useState<Record<NumberFieldKey, string>>(() => ({
    editorMaxRows: String(current.editorMaxRows),
    queryTimeoutSecs: String(Math.ceil(current.queryTimeoutMs / 1000)),
    historyMaxPerConnection: String(current.historyMaxPerConnection),
    keepaliveSecs: String(current.keepaliveSecs),
    planHistoryMaxPerConnection: String(current.planHistoryMaxPerConnection),
  }));
  const [historyEnabled, setHistoryEnabled] = useState(current.historyEnabled);
  const [autoReconnect, setAutoReconnect] = useState(current.autoReconnect);
  const [planHistoryEnabled, setPlanHistoryEnabled] = useState(current.planHistoryEnabled);
  const [submitted, setSubmitted] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const logDir = useQuery({ queryKey: ['logDir'], queryFn: ipc.logDir, staleTime: Infinity });

  const rowsValue = parseBounded(rows, ROWS_RANGE);
  const cellsValue = parseBounded(cells, CELLS_RANGE);
  const language: Language = i18n.language.startsWith('vi') ? 'vi' : 'en';
  const parsed = {
    editorMaxRows: parseNumberField('editorMaxRows', numbers.editorMaxRows),
    queryTimeoutSecs: parseNumberField('queryTimeoutSecs', numbers.queryTimeoutSecs),
    historyMaxPerConnection: parseNumberField(
      'historyMaxPerConnection',
      numbers.historyMaxPerConnection,
    ),
    keepaliveSecs: parseNumberField('keepaliveSecs', numbers.keepaliveSecs),
    planHistoryMaxPerConnection: parseNumberField(
      'planHistoryMaxPerConnection',
      numbers.planHistoryMaxPerConnection,
    ),
  };

  const numberField = (key: NumberFieldKey) => {
    const { min, max, zeroIsOff } = NUMBER_FIELDS[key];
    const invalid = submitted && parsed[key] === null;
    const message = zeroIsOff
      ? t('validation.rangeOrZero', { min, max })
      : t('validation.range', { min, max });
    return (
      <Field
        label={t(`number.${key}`)}
        htmlFor={`${id}-${key}`}
        error={invalid ? message : undefined}
      >
        <Input
          id={`${id}-${key}`}
          inputMode="numeric"
          value={numbers[key]}
          aria-invalid={invalid}
          onChange={(e) => setNumbers((n) => ({ ...n, [key]: e.target.value }))}
        />
      </Field>
    );
  };

  const submit = async () => {
    setSubmitted(true);
    const {
      editorMaxRows,
      queryTimeoutSecs,
      historyMaxPerConnection,
      keepaliveSecs,
      planHistoryMaxPerConnection,
    } = parsed;
    if (
      saving ||
      rowsValue === null ||
      cellsValue === null ||
      editorMaxRows === null ||
      queryTimeoutSecs === null ||
      historyMaxPerConnection === null ||
      keepaliveSecs === null ||
      planHistoryMaxPerConnection === null
    )
      return;
    setSaving(true);
    setError(null);
    try {
      // Other screens write settings too (close-to-tray, debugger): start from the stored values,
      // not from the copy this dialog read when it opened.
      const stored = await ipc.settingsGet().catch(() => current);
      const next: AppSettings = {
        ...stored,
        logLevel,
        gridMaxRows: rowsValue,
        gridMaxCells: cellsValue,
        timestampDisplay: timestamp,
        planAdvisorMetadata: advisorMetadata,
        editorMaxRows,
        queryTimeoutMs: queryTimeoutSecs * 1000,
        historyEnabled,
        historyMaxPerConnection,
        autoReconnect,
        keepaliveSecs,
        planHistoryEnabled,
        planHistoryMaxPerConnection,
        // Mirrors the interface language so OS notifications use it too.
        language,
      };
      await save(next);
      useAppearanceStore.getState().update(appearance);
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
          <AppearanceFields id={id} value={appearance} onChange={setAppearance} />
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

        <Section title={t('section.editor')}>
          <div className="grid grid-cols-2 gap-2">
            {numberField('editorMaxRows')}
            {numberField('queryTimeoutSecs')}
          </div>
          <p className="text-xs text-muted-foreground">{t('queryTimeoutHint')}</p>
          <Toggle
            id={`${id}-history`}
            label={t('historyEnabled')}
            checked={historyEnabled}
            onCheckedChange={setHistoryEnabled}
          />
          {numberField('historyMaxPerConnection')}
        </Section>

        <Section title={t('section.connection')}>
          <Toggle
            id={`${id}-reconnect`}
            label={t('autoReconnect')}
            hint={t('autoReconnectHint')}
            checked={autoReconnect}
            onCheckedChange={setAutoReconnect}
          />
          {numberField('keepaliveSecs')}
          <p className="text-xs text-muted-foreground">{t('keepaliveHint')}</p>
        </Section>

        <Section title={t('section.plan')}>
          <Toggle
            id={`${id}-advisor`}
            label={t('planAdvisorMetadata')}
            hint={t('planAdvisorMetadataHint')}
            checked={advisorMetadata}
            onCheckedChange={setAdvisorMetadata}
          />
          <Toggle
            id={`${id}-plan-history`}
            label={t('planHistoryEnabled')}
            checked={planHistoryEnabled}
            onCheckedChange={setPlanHistoryEnabled}
          />
          {numberField('planHistoryMaxPerConnection')}
        </Section>

        <Section title={t('section.background')}>
          <BackgroundSettings />
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

function Toggle({
  id,
  label,
  hint,
  checked,
  onCheckedChange,
}: {
  id: string;
  label: string;
  hint?: string;
  checked: boolean;
  onCheckedChange: (checked: boolean) => void;
}) {
  return (
    <div className="flex items-start gap-2">
      <Switch id={id} checked={checked} onCheckedChange={onCheckedChange} className="mt-0.5" />
      <div className="flex min-w-0 flex-col gap-0.5">
        <Label htmlFor={id}>{label}</Label>
        {hint && <p className="text-xs text-muted-foreground">{hint}</p>}
      </div>
    </div>
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

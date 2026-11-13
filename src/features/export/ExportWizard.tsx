import { useQuery, useQueryClient } from '@tanstack/react-query';
import { ChevronLeft, ChevronRight, FolderOpen, Save, Trash2 } from 'lucide-react';
import { useMemo, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Checkbox } from '@/components/ui/Checkbox';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';
import { Select } from '@/components/ui/Select';
import { Textarea } from '@/components/ui/Textarea';
import { toast } from '@/components/ui/toastStore';
import { useJobsStore } from '@/features/jobs/jobsStore';
import { ipc } from '@/lib/ipc';
import type { ExportContent, ExportSource, ObjectPath } from '@/lib/types';
import type { ExportOrigin } from './open';
import {
  buildSpec,
  defaultForm,
  extensionFor,
  formFromSpec,
  ipcMessage,
  formatsFor,
  saveFilters,
  sourceFromPath,
  suggestedName,
  validate,
  type ExportForm,
  type FormatKind,
} from './specForm';

export interface ExportWizardProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  origin: ExportOrigin;
}

const STEPS = ['source', 'format', 'destination'] as const;
type Step = (typeof STEPS)[number];

function Field({
  label,
  htmlFor,
  children,
}: {
  label: string;
  htmlFor?: string;
  children: ReactNode;
}) {
  return (
    <div className="space-y-1">
      <Label {...(htmlFor ? { htmlFor } : {})}>{label}</Label>
      {children}
    </div>
  );
}

function Check({
  id,
  label,
  checked,
  onChange,
}: {
  id: string;
  label: string;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <div className="flex items-center gap-2 text-[13px]">
      <Checkbox id={id} checked={checked} onCheckedChange={(v) => onChange(v === true)} />
      <label htmlFor={id}>{label}</label>
    </div>
  );
}

function lastName(path: ObjectPath | undefined): string {
  return path?.segments[path.segments.length - 1]?.name ?? '';
}

function pathKey(p: ObjectPath): string {
  return p.segments.map((s) => `${s.kind}:${s.name}`).join('/');
}

/** Tables of a database / schema, to pick from. */
function useTables(origin: ExportOrigin, scope: ObjectPath | undefined, enabled: boolean) {
  return useQuery({
    queryKey: ['exportTables', origin.session.sessionId, scope ? pathKey(scope) : ''],
    enabled: enabled && !!scope,
    queryFn: () =>
      ipc.schemaModel(origin.session.sessionId, {
        scope: scope as ObjectPath,
        includeViews: true,
        detail: 'keys',
      }),
  });
}

function SourceStep({
  form,
  setForm,
  origin,
  all,
}: {
  form: ExportForm;
  setForm: (f: ExportForm) => void;
  origin: ExportOrigin;
  all: ExportSource;
}) {
  const { t } = useTranslation('export');
  const scoped = all.kind === 'database' || all.kind === 'schema';
  const picking = scoped && form.source.kind === 'tables';
  const scope = scoped ? all.path : undefined;
  const tables = useTables(origin, scope, picking);
  const selected = new Set(form.source.kind === 'tables' ? form.source.paths.map(pathKey) : []);
  const toggle = (path: ObjectPath, on: boolean) => {
    const current = form.source.kind === 'tables' ? form.source.paths : [];
    const paths = on ? [...current, path] : current.filter((p) => pathKey(p) !== pathKey(path));
    setForm({ ...form, source: { kind: 'tables', paths } });
  };
  return (
    <div className="space-y-3">
      {form.source.kind === 'query' && (
        <Field label={t('source.query')} htmlFor="export-query">
          <Textarea
            id="export-query"
            rows={5}
            value={form.source.text}
            placeholder={t('source.queryPlaceholder')}
            onChange={(e) =>
              setForm({ ...form, source: { kind: 'query', text: e.target.value, database: null } })
            }
          />
          <p className="text-xs text-muted-foreground">{t('source.queryHint')}</p>
        </Field>
      )}
      {scoped && (
        <fieldset className="space-y-1.5">
          <legend className="text-xs font-medium text-muted-foreground">
            {t('source.objects')}
          </legend>
          <label className="flex items-center gap-2 text-[13px]">
            <input
              type="radio"
              name="scope"
              checked={!picking}
              onChange={() => setForm({ ...form, source: all })}
            />
            {all.kind === 'database' ? t('source.allDatabase') : t('source.allSchema')}
          </label>
          <label className="flex items-center gap-2 text-[13px]">
            <input
              type="radio"
              name="scope"
              checked={picking}
              onChange={() => setForm({ ...form, source: { kind: 'tables', paths: [] } })}
            />
            {t('source.chooseTables')}
          </label>
          {picking && (
            <ul
              data-testid="export-tables"
              className="max-h-40 overflow-auto rounded-md border border-border p-1.5"
            >
              {tables.isLoading && (
                <li className="text-xs text-muted-foreground">{t('loading')}</li>
              )}
              {tables.isError && (
                <li className="text-xs text-danger">{ipcMessage(tables.error)}</li>
              )}
              {tables.data?.tables.map((m) => (
                <li key={pathKey(m.path)}>
                  <Check
                    id={`export-table-${m.name}`}
                    label={m.schema ? `${m.schema}.${m.name}` : m.name}
                    checked={selected.has(pathKey(m.path))}
                    onChange={(on) => toggle(m.path, on)}
                  />
                </li>
              ))}
            </ul>
          )}
        </fieldset>
      )}
      {!scoped && form.source.kind === 'tables' && (
        <Field label={t('source.objects')}>
          <p className="text-[13px]">{form.source.paths.map(lastName).join(', ')}</p>
        </Field>
      )}
      {form.source.kind !== 'query' && (
        <Field label={t('content.label')} htmlFor="export-content">
          <Select
            id="export-content"
            aria-label={t('content.label')}
            value={form.content}
            onValueChange={(v) => setForm({ ...form, content: v as ExportContent })}
            options={(['both', 'data', 'schema'] as const).map((c) => ({
              value: c,
              label: t(`content.${c}`),
            }))}
          />
        </Field>
      )}
    </div>
  );
}

function FormatStep({
  form,
  setForm,
  origin,
}: {
  form: ExportForm;
  setForm: (f: ExportForm) => void;
  origin: ExportOrigin;
}) {
  const { t } = useTranslation('export');
  const kinds = formatsFor(origin.session.dialect);
  return (
    <div className="space-y-3">
      <Field label={t('format.label')} htmlFor="export-format">
        <Select
          id="export-format"
          aria-label={t('format.label')}
          value={form.format}
          onValueChange={(v) => setForm({ ...form, format: v as FormatKind })}
          options={kinds.map((k) => ({ value: k, label: t(`format.${k}`) }))}
        />
      </Field>
      {form.format === 'csv' && (
        <div className="grid grid-cols-2 gap-3">
          <Field label={t('csv.delimiter')} htmlFor="csv-delimiter">
            <Input
              id="csv-delimiter"
              maxLength={1}
              value={form.csv.delimiter ?? ','}
              onChange={(e) =>
                setForm({ ...form, csv: { ...form.csv, delimiter: e.target.value } })
              }
            />
          </Field>
          <Field label={t('csv.quote')} htmlFor="csv-quote">
            <Input
              id="csv-quote"
              maxLength={1}
              value={form.csv.quote ?? '"'}
              onChange={(e) => setForm({ ...form, csv: { ...form.csv, quote: e.target.value } })}
            />
          </Field>
          <Field label={t('csv.nullText')} htmlFor="csv-null">
            <Input
              id="csv-null"
              value={form.csv.nullText ?? ''}
              onChange={(e) => setForm({ ...form, csv: { ...form.csv, nullText: e.target.value } })}
            />
          </Field>
          <Field label={t('csv.lineEnding')} htmlFor="csv-eol">
            <Select
              id="csv-eol"
              aria-label={t('csv.lineEnding')}
              value={form.csv.lineEnding ?? 'crlf'}
              onValueChange={(v) =>
                setForm({ ...form, csv: { ...form.csv, lineEnding: v as 'lf' | 'crlf' } })
              }
              options={[
                { value: 'crlf', label: 'CRLF (Windows)' },
                { value: 'lf', label: 'LF (Unix)' },
              ]}
            />
          </Field>
          <Check
            id="csv-bom"
            label={t('csv.bom')}
            checked={form.csv.encoding === 'utf8Bom'}
            onChange={(on) =>
              setForm({ ...form, csv: { ...form.csv, encoding: on ? 'utf8Bom' : 'utf8' } })
            }
          />
          <Check
            id="csv-header"
            label={t('csv.header')}
            checked={form.csv.header ?? true}
            onChange={(on) => setForm({ ...form, csv: { ...form.csv, header: on } })}
          />
          <Check
            id="csv-excel-safe"
            label={t('csv.excelSafe')}
            checked={form.csv.excelSafe ?? false}
            onChange={(on) => setForm({ ...form, csv: { ...form.csv, excelSafe: on } })}
          />
          <Check
            id="csv-quote-all"
            label={t('csv.quoteAll')}
            checked={form.csv.quoteAll ?? false}
            onChange={(on) => setForm({ ...form, csv: { ...form.csv, quoteAll: on } })}
          />
        </div>
      )}
      {form.format === 'json' && (
        <div className="space-y-2">
          <Field label={t('json.layout')} htmlFor="json-layout">
            <Select
              id="json-layout"
              aria-label={t('json.layout')}
              value={form.json.layout ?? 'array'}
              onValueChange={(v) =>
                setForm({ ...form, json: { ...form.json, layout: v as 'array' | 'ndjson' } })
              }
              options={[
                { value: 'array', label: t('json.array') },
                { value: 'ndjson', label: t('json.ndjson') },
              ]}
            />
          </Field>
          <Check
            id="json-pretty"
            label={t('json.pretty')}
            checked={form.json.pretty ?? false}
            onChange={(on) => setForm({ ...form, json: { ...form.json, pretty: on } })}
          />
        </div>
      )}
      {form.format === 'xlsx' && (
        <div className="space-y-2">
          <Check
            id="xlsx-header"
            label={t('xlsx.header')}
            checked={form.xlsx.header ?? true}
            onChange={(on) => setForm({ ...form, xlsx: { ...form.xlsx, header: on } })}
          />
          <Check
            id="xlsx-freeze"
            label={t('xlsx.freeze')}
            checked={form.xlsx.freezeHeader ?? true}
            onChange={(on) => setForm({ ...form, xlsx: { ...form.xlsx, freezeHeader: on } })}
          />
          <Check
            id="xlsx-dictionary"
            label={t('xlsx.dictionary')}
            checked={form.xlsx.dataDictionary ?? false}
            onChange={(on) => setForm({ ...form, xlsx: { ...form.xlsx, dataDictionary: on } })}
          />
          <p className="text-xs text-muted-foreground">{t('xlsx.splitHint')}</p>
        </div>
      )}
      {form.format === 'sql' && (
        <div className="space-y-2">
          <Field label={t('sql.batch')} htmlFor="sql-batch">
            <Input
              id="sql-batch"
              type="number"
              min={1}
              value={form.sql.insertBatchRows ?? 100}
              onChange={(e) =>
                setForm({
                  ...form,
                  sql: { ...form.sql, insertBatchRows: Math.max(1, Number(e.target.value) || 1) },
                })
              }
            />
          </Field>
          <Check
            id="sql-columns"
            label={t('sql.columnNames')}
            checked={form.sql.includeColumnNames ?? true}
            onChange={(on) => setForm({ ...form, sql: { ...form.sql, includeColumnNames: on } })}
          />
          <Check
            id="sql-tx"
            label={t('sql.transaction')}
            checked={form.sql.wrapInTransaction ?? true}
            onChange={(on) => setForm({ ...form, sql: { ...form.sql, wrapInTransaction: on } })}
          />
          <Check
            id="sql-fk"
            label={t('sql.disableFk')}
            checked={form.sql.disableFkChecks ?? false}
            onChange={(on) => setForm({ ...form, sql: { ...form.sql, disableFkChecks: on } })}
          />
          <Check
            id="sql-qualify"
            label={t('sql.qualify')}
            checked={form.sql.ddl?.qualifySchema ?? true}
            onChange={(on) =>
              setForm({
                ...form,
                sql: { ...form.sql, ddl: { ...form.sql.ddl, qualifySchema: on } },
              })
            }
          />
          <Check
            id="sql-drop"
            label={t('sql.drop')}
            checked={form.sql.ddl?.includeDrop ?? false}
            onChange={(on) =>
              setForm({ ...form, sql: { ...form.sql, ddl: { ...form.sql.ddl, includeDrop: on } } })
            }
          />
        </div>
      )}
      {(form.format === 'csv' || form.format === 'xlsx' || form.format === 'json') && (
        <Field label={t('binary.label')} htmlFor="export-binary">
          <Select
            id="export-binary"
            aria-label={t('binary.label')}
            value={form.binary}
            onValueChange={(v) => setForm({ ...form, binary: v as 'base64' | 'hex' })}
            options={[
              { value: 'base64', label: 'Base64' },
              { value: 'hex', label: 'Hex' },
            ]}
          />
        </Field>
      )}
    </div>
  );
}

function DestinationStep({
  form,
  setForm,
  origin,
  label,
}: {
  form: ExportForm;
  setForm: (f: ExportForm) => void;
  origin: ExportOrigin;
  label: string;
}) {
  const { t } = useTranslation('export');
  const client = useQueryClient();
  const connectionId = origin.session.connectionId;
  const presets = useQuery({
    queryKey: ['exportPresets', connectionId],
    queryFn: () => ipc.exportPresetsList(connectionId),
  });
  const [presetName, setPresetName] = useState('');
  const [presetId, setPresetId] = useState<string>('');
  const pick = async () => {
    const names = {
      zip: t('file.zip'),
      sql: t('file.sql'),
      csv: t('file.csv'),
      xlsx: t('file.xlsx'),
      json: t('file.json'),
    };
    const path = await ipc.filePickSave(suggestedName(form, label), saveFilters(form, names));
    if (path) setForm({ ...form, path });
  };
  const apply = (id: string) => {
    setPresetId(id);
    const preset = presets.data?.find((p) => p.id === id);
    const restored = preset ? formFromSpec(preset.spec, form) : null;
    if (restored) setForm(restored);
  };
  const save = async () => {
    const name = presetName.trim();
    if (!name) return;
    const spec = buildSpec(connectionId, form);
    await ipc.exportPresetSave({
      id: crypto.randomUUID(),
      name,
      connectionId,
      spec,
      createdAt: '',
      updatedAt: '',
    });
    setPresetName('');
    await client.invalidateQueries({ queryKey: ['exportPresets', connectionId] });
    toast.success(t('preset.saved'));
  };
  const remove = async () => {
    if (!presetId) return;
    await ipc.exportPresetDelete(presetId);
    setPresetId('');
    await client.invalidateQueries({ queryKey: ['exportPresets', connectionId] });
  };
  return (
    <div className="space-y-3">
      <Field label={t('destination.file')} htmlFor="export-path">
        <div className="flex gap-2">
          <Input id="export-path" readOnly value={form.path} placeholder={t('destination.none')} />
          <Button onClick={() => void pick()}>
            <FolderOpen size={13} />
            {t('destination.browse')}
          </Button>
        </div>
        <p className="text-xs text-muted-foreground">
          {t('destination.hint', { ext: extensionFor(form) })}
        </p>
      </Field>
      <div className="space-y-1.5 border-t border-border pt-3">
        <p className="text-xs font-medium text-muted-foreground">{t('preset.title')}</p>
        {(presets.data?.length ?? 0) > 0 && (
          <div className="flex gap-2">
            <Select
              aria-label={t('preset.apply')}
              value={presetId || undefined}
              placeholder={t('preset.apply')}
              onValueChange={apply}
              options={(presets.data ?? []).map((p) => ({ value: p.id, label: p.name }))}
            />
            <Button
              variant="outline"
              aria-label={t('preset.delete')}
              disabled={!presetId}
              onClick={() => void remove()}
            >
              <Trash2 size={13} />
            </Button>
          </div>
        )}
        <div className="flex gap-2">
          <Input
            aria-label={t('preset.name')}
            placeholder={t('preset.name')}
            value={presetName}
            onChange={(e) => setPresetName(e.target.value)}
          />
          <Button disabled={presetName.trim() === ''} onClick={() => void save()}>
            <Save size={13} />
            {t('preset.save')}
          </Button>
        </div>
      </div>
    </div>
  );
}

/** Export wizard: what to export, in which format, where to. Starts a background job. */
export function ExportWizard({ open, onOpenChange, origin }: ExportWizardProps) {
  const { t } = useTranslation('export');
  const all = useMemo(() => sourceFromPath(origin.path, origin.query), [origin]);
  const [form, setForm] = useState<ExportForm>(() => defaultForm(all));
  const [step, setStep] = useState<Step>('source');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const index = STEPS.indexOf(step);
  const issues = validate(form);
  const stepBlocked =
    (step === 'source' && (issues.includes('noTables') || issues.includes('noQuery'))) ||
    (step === 'format' &&
      (issues.includes('delimiterLength') ||
        issues.includes('quoteLength') ||
        issues.includes('delimiterEqualsQuote')));
  const label = lastName(origin.path) || t('source.queryName');

  const start = async () => {
    setBusy(true);
    setError(null);
    try {
      await ipc.exportStart(buildSpec(origin.session.connectionId, form));
      toast.info(t('started'));
      useJobsStore.getState().setPanelOpen(true);
      onOpenChange(false);
    } catch (e) {
      setError(ipcMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        title={t('title')}
        description={t(`step.${step}`, { n: index + 1, total: STEPS.length })}
        className="w-[min(560px,94vw)]"
        footer={
          <>
            <Button disabled={index === 0} onClick={() => setStep(STEPS[index - 1] ?? 'source')}>
              <ChevronLeft size={13} />
              {t('back')}
            </Button>
            {step !== 'destination' ? (
              <Button
                variant="primary"
                disabled={stepBlocked}
                onClick={() => setStep(STEPS[index + 1] ?? 'destination')}
              >
                {t('next')}
                <ChevronRight size={13} />
              </Button>
            ) : (
              <Button
                variant="primary"
                disabled={busy || issues.length > 0}
                onClick={() => void start()}
              >
                {t('start')}
              </Button>
            )}
          </>
        }
      >
        {step === 'source' && (
          <SourceStep form={form} setForm={setForm} origin={origin} all={all} />
        )}
        {step === 'format' && <FormatStep form={form} setForm={setForm} origin={origin} />}
        {step === 'destination' && (
          <DestinationStep form={form} setForm={setForm} origin={origin} label={label} />
        )}
        {error && (
          <p role="alert" className="mt-3 select-text break-words text-xs text-danger">
            {error}
          </p>
        )}
      </DialogContent>
    </Dialog>
  );
}

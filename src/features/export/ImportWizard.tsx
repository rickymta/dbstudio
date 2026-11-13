import { useQuery } from '@tanstack/react-query';
import { ChevronLeft, ChevronRight, FolderOpen } from 'lucide-react';
import { useMemo, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Checkbox } from '@/components/ui/Checkbox';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';
import { Select } from '@/components/ui/Select';
import { toast } from '@/components/ui/toastStore';
import { useJobsStore } from '@/features/jobs/jobsStore';
import { ipc } from '@/lib/ipc';
import type {
  ConfirmationRequest,
  FileFilter,
  ImportPreview,
  IpcError,
  ObjectPath,
  OnConflict,
  SessionInfo,
  Transform,
  Value,
} from '@/lib/types';
import {
  autoMapping,
  buildImportSpec,
  defaultImportForm,
  importFormat,
  kindFromFile,
  layoutFromFile,
  newColumnsFromPreview,
  validateImport,
  type ImportForm,
  type ImportKind,
  type TargetMode,
} from './importForm';
import { ipcMessage } from './specForm';

export interface ImportWizardProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  session: SessionInfo;
  /** Table to import into; omitted = new table or SQL script. */
  target?: ObjectPath;
  /** Schema / database node whose tables can be chosen and where new tables are created. */
  parent?: ObjectPath;
}

const TRANSFORMS: Transform[] = ['none', 'trim', 'emptyAsNull', 'trimEmptyAsNull'];
const SKIP = '__skip__';

function pathKey(p: ObjectPath): string {
  return p.segments.map((s) => `${s.kind}:${s.name}`).join('/');
}

/** Schema / database node that contains `table`. */
function parentOf(table: ObjectPath): ObjectPath {
  const segments = [...table.segments];
  while (segments.length > 0 && ['table', 'view', 'folder'].includes(segments.at(-1)?.kind ?? '')) {
    segments.pop();
  }
  return { segments };
}

function valueText(v: Value): string {
  switch (v.t) {
    case 'null':
      return '';
    case 'text':
    case 'decimal':
    case 'date':
    case 'time':
    case 'dateTime':
    case 'timestamp':
    case 'uuid':
    case 'json':
      return String(v.v);
    case 'ext':
      return v.v.text;
    default:
      return JSON.stringify(v.v);
  }
}

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

function PreviewTable({ preview }: { preview: ImportPreview }) {
  const { t } = useTranslation('export');
  return (
    <div className="space-y-1">
      <p className="text-xs text-muted-foreground">
        {t('import.previewInfo', {
          columns: preview.columns.length,
          rows: preview.estimatedRows ?? '?',
        })}
      </p>
      <div className="max-h-40 overflow-auto rounded-md border border-border">
        <table data-testid="import-preview" className="w-full text-xs">
          <thead className="sticky top-0 bg-surface-muted">
            <tr>
              {preview.columns.map((c) => (
                <th key={c} className="px-2 py-1 text-left font-medium">
                  {c}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {preview.rows.slice(0, 8).map((r, i) => (
              <tr key={i} className="border-t border-border">
                {r.map((v, j) => (
                  <td key={j} className="max-w-40 truncate px-2 py-0.5">
                    {valueText(v)}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

function FileStep({
  form,
  setForm,
  preview,
  loading,
  error,
}: {
  form: ImportForm;
  setForm: (f: ImportForm) => void;
  preview: ImportPreview | undefined;
  loading: boolean;
  error: string | null;
}) {
  const { t } = useTranslation('export');
  const pick = async () => {
    const filters: FileFilter[] = [
      {
        name: t('import.fileTypes'),
        extensions: ['csv', 'tsv', 'txt', 'xlsx', 'json', 'ndjson', 'sql'],
      },
    ];
    const file = await ipc.filePickOpen(filters, false);
    if (!file) return;
    const kind = kindFromFile(file);
    const next: ImportForm = {
      ...form,
      file,
      kind,
      json: { layout: layoutFromFile(file) },
      mapping: [],
      newColumns: [],
      xlsx: { ...form.xlsx, sheet: '' },
    };
    // Look at the file once to learn its delimiter / sheets.
    if (kind === 'csv' || kind === 'xlsx') {
      try {
        const p = await ipc.importPreview({ file, format: importFormat(next), rows: 5 });
        if (kind === 'csv' && p.detectedDelimiter) {
          next.csv = { ...next.csv, delimiter: p.detectedDelimiter };
        }
        if (kind === 'xlsx' && p.sheets[0]) next.xlsx = { ...next.xlsx, sheet: p.sheets[0] };
      } catch {
        // The preview below reports the problem.
      }
    }
    setForm(next);
  };
  const csv = form.csv;
  return (
    <div className="space-y-3">
      <Field label={t('import.file')} htmlFor="import-file">
        <div className="flex gap-2">
          <Input id="import-file" readOnly value={form.file} placeholder={t('import.noFile')} />
          <Button onClick={() => void pick()}>
            <FolderOpen size={13} />
            {t('destination.browse')}
          </Button>
        </div>
      </Field>
      {form.file !== '' && (
        <>
          <Field label={t('import.kind')} htmlFor="import-kind">
            <Select
              id="import-kind"
              aria-label={t('import.kind')}
              value={form.kind}
              onValueChange={(v) =>
                setForm({ ...form, kind: v as ImportKind, mapping: [], newColumns: [] })
              }
              options={(['csv', 'xlsx', 'json', 'sql'] as const).map((k) => ({
                value: k,
                label: t(`import.kinds.${k}`),
              }))}
            />
          </Field>
          {form.kind === 'csv' && (
            <div className="grid grid-cols-3 items-end gap-3">
              <Field label={t('csv.delimiter')} htmlFor="import-delimiter">
                <Input
                  id="import-delimiter"
                  maxLength={1}
                  value={csv.delimiter}
                  onChange={(e) =>
                    setForm({ ...form, csv: { ...csv, delimiter: e.target.value || ',' } })
                  }
                />
              </Field>
              <Field label={t('csv.quote')} htmlFor="import-quote">
                <Input
                  id="import-quote"
                  maxLength={1}
                  value={csv.quote}
                  onChange={(e) =>
                    setForm({ ...form, csv: { ...csv, quote: e.target.value || '"' } })
                  }
                />
              </Field>
              <div className="flex items-center gap-2 pb-1 text-[13px]">
                <Checkbox
                  id="import-header"
                  checked={csv.header}
                  onCheckedChange={(v) =>
                    setForm({
                      ...form,
                      csv: { ...csv, header: v === true },
                      mapping: [],
                      newColumns: [],
                    })
                  }
                />
                <label htmlFor="import-header">{t('import.header')}</label>
              </div>
            </div>
          )}
          {form.kind === 'xlsx' && (
            <div className="grid grid-cols-2 items-end gap-3">
              <Field label={t('import.sheet')} htmlFor="import-sheet">
                <Select
                  id="import-sheet"
                  aria-label={t('import.sheet')}
                  value={form.xlsx.sheet || preview?.sheets[0] || undefined}
                  onValueChange={(v) =>
                    setForm({
                      ...form,
                      xlsx: { ...form.xlsx, sheet: v },
                      mapping: [],
                      newColumns: [],
                    })
                  }
                  options={(preview?.sheets ?? []).map((s) => ({ value: s, label: s }))}
                />
              </Field>
              <div className="flex items-center gap-2 pb-1 text-[13px]">
                <Checkbox
                  id="import-xlsx-header"
                  checked={form.xlsx.header}
                  onCheckedChange={(v) =>
                    setForm({
                      ...form,
                      xlsx: { ...form.xlsx, header: v === true },
                      mapping: [],
                      newColumns: [],
                    })
                  }
                />
                <label htmlFor="import-xlsx-header">{t('import.header')}</label>
              </div>
            </div>
          )}
          {form.kind === 'json' && (
            <Field label={t('json.layout')} htmlFor="import-json-layout">
              <Select
                id="import-json-layout"
                aria-label={t('json.layout')}
                value={form.json.layout}
                onValueChange={(v) =>
                  setForm({
                    ...form,
                    json: { layout: v as 'array' | 'ndjson' },
                    mapping: [],
                    newColumns: [],
                  })
                }
                options={[
                  { value: 'array', label: t('json.array') },
                  { value: 'ndjson', label: t('json.ndjson') },
                ]}
              />
            </Field>
          )}
          {form.kind === 'sql' && (
            <div className="flex items-center gap-2 text-[13px]">
              <Checkbox
                id="import-continue"
                checked={form.sql.continueOnError}
                onCheckedChange={(v) => setForm({ ...form, sql: { continueOnError: v === true } })}
              />
              <label htmlFor="import-continue">{t('import.continueOnError')}</label>
            </div>
          )}
          {loading && <p className="text-xs text-muted-foreground">{t('loading')}</p>}
          {error && (
            <p role="alert" className="select-text break-words text-xs text-danger">
              {error}
            </p>
          )}
          {preview && form.kind !== 'sql' && <PreviewTable preview={preview} />}
          {preview && form.kind === 'sql' && (
            <ul data-testid="import-statements" className="max-h-40 overflow-auto text-xs">
              {preview.statementsPreview.map((s) => (
                <li key={s.index} className="flex gap-2 border-b border-border py-0.5">
                  <span className={s.destructive ? 'font-medium text-danger' : ''}>{s.verb}</span>
                  <span className="truncate text-muted-foreground">{s.targets.join(', ')}</span>
                </li>
              ))}
            </ul>
          )}
        </>
      )}
    </div>
  );
}

function TargetStep({
  form,
  setForm,
  session,
  parent,
}: {
  form: ImportForm;
  setForm: (f: ImportForm) => void;
  session: SessionInfo;
  parent: ObjectPath | null;
}) {
  const { t } = useTranslation('export');
  const tables = useQuery({
    queryKey: ['importTables', session.sessionId, parent ? pathKey(parent) : ''],
    enabled: !!parent,
    queryFn: () =>
      ipc.schemaModel(session.sessionId, {
        scope: parent as ObjectPath,
        includeViews: false,
        detail: 'keys',
      }),
  });
  const options = (tables.data?.tables ?? []).map((m) => ({
    value: pathKey(m.path),
    label: m.schema ? `${m.schema}.${m.name}` : m.name,
  }));
  const byKey = new Map((tables.data?.tables ?? []).map((m) => [pathKey(m.path), m.path]));
  const chosen = form.table ? pathKey(form.table) : undefined;
  const withChosen =
    chosen && !options.some((o) => o.value === chosen) && form.table
      ? [...options, { value: chosen, label: form.table.segments.at(-1)?.name ?? chosen }]
      : options;
  return (
    <div className="space-y-3">
      <fieldset className="space-y-1.5">
        <legend className="text-xs font-medium text-muted-foreground">{t('import.target')}</legend>
        {(['existing', 'new'] as TargetMode[]).map((mode) => (
          <label key={mode} className="flex items-center gap-2 text-[13px]">
            <input
              type="radio"
              name="target"
              checked={form.targetMode === mode}
              onChange={() => setForm({ ...form, targetMode: mode, mapping: [], newColumns: [] })}
            />
            {t(`import.targetMode.${mode}`)}
          </label>
        ))}
      </fieldset>
      {form.targetMode === 'existing' ? (
        <Field label={t('import.table')} htmlFor="import-table">
          <Select
            id="import-table"
            aria-label={t('import.table')}
            value={chosen}
            placeholder={t('import.chooseTable')}
            onValueChange={(v) =>
              setForm({ ...form, table: byKey.get(v) ?? form.table, mapping: [] })
            }
            options={withChosen}
          />
        </Field>
      ) : (
        <Field label={t('import.newName')} htmlFor="import-new-name">
          <Input
            id="import-new-name"
            value={form.newName}
            onChange={(e) => setForm({ ...form, newName: e.target.value })}
          />
        </Field>
      )}
    </div>
  );
}

function MappingStep({
  form,
  setForm,
  preview,
  tableColumns,
}: {
  form: ImportForm;
  setForm: (f: ImportForm) => void;
  preview: ImportPreview;
  tableColumns: string[];
}) {
  const { t } = useTranslation('export');
  const setRow = (i: number, patch: Partial<ImportForm['mapping'][number]>) =>
    setForm({
      ...form,
      mapping: form.mapping.map((m, j) => (j === i ? { ...m, ...patch } : m)),
    });
  return (
    <div className="space-y-3">
      {form.targetMode === 'existing' ? (
        <table data-testid="import-mapping" className="w-full text-xs">
          <thead>
            <tr className="text-left text-muted-foreground">
              <th className="py-1 font-medium">{t('import.fileColumn')}</th>
              <th className="py-1 font-medium">{t('import.tableColumn')}</th>
              <th className="py-1 font-medium">{t('import.transform')}</th>
            </tr>
          </thead>
          <tbody>
            {form.mapping.map((m, i) => (
              <tr key={m.source + i} className="border-t border-border">
                <td className="py-1 pr-2">{m.source}</td>
                <td className="py-1 pr-2">
                  <Select
                    aria-label={t('import.mapTo', { column: m.source })}
                    value={m.target ?? SKIP}
                    onValueChange={(v) => setRow(i, { target: v === SKIP ? null : v })}
                    options={[
                      { value: SKIP, label: t('import.skip') },
                      ...tableColumns.map((c) => ({ value: c, label: c })),
                    ]}
                  />
                </td>
                <td className="py-1">
                  <Select
                    aria-label={t('import.transformOf', { column: m.source })}
                    value={m.transform}
                    onValueChange={(v) => setRow(i, { transform: v as Transform })}
                    options={TRANSFORMS.map((x) => ({
                      value: x,
                      label: t(`import.transforms.${x}`),
                    }))}
                  />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : (
        <table data-testid="import-new-columns" className="w-full text-xs">
          <thead>
            <tr className="text-left text-muted-foreground">
              <th className="py-1 font-medium">{t('import.fileColumn')}</th>
              <th className="py-1 font-medium">{t('import.columnName')}</th>
              <th className="py-1 font-medium">{t('import.columnType')}</th>
              <th className="py-1 font-medium">{t('import.pk')}</th>
            </tr>
          </thead>
          <tbody>
            {form.newColumns.map((c, i) => {
              const patch = (p: Partial<typeof c>) =>
                setForm({
                  ...form,
                  newColumns: form.newColumns.map((x, j) => (j === i ? { ...x, ...p } : x)),
                });
              return (
                <tr key={i} className="border-t border-border">
                  <td className="py-1 pr-2">{preview.columns[i]}</td>
                  <td className="py-1 pr-2">
                    <Input
                      aria-label={t('import.columnName') + ` ${i + 1}`}
                      value={c.name}
                      onChange={(e) => patch({ name: e.target.value })}
                    />
                  </td>
                  <td className="py-1 pr-2">
                    <Input
                      aria-label={t('import.columnType') + ` ${i + 1}`}
                      value={c.typeName}
                      onChange={(e) => patch({ typeName: e.target.value })}
                    />
                  </td>
                  <td className="py-1">
                    <Checkbox
                      aria-label={t('import.pk') + ` ${i + 1}`}
                      checked={c.primaryKey}
                      onCheckedChange={(v) =>
                        patch({ primaryKey: v === true, nullable: v !== true })
                      }
                    />
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
      <div className="grid grid-cols-2 gap-3 border-t border-border pt-3">
        {form.targetMode === 'existing' && (
          <Field label={t('import.onConflict')} htmlFor="import-conflict">
            <Select
              id="import-conflict"
              aria-label={t('import.onConflict')}
              value={form.onConflict ?? 'error'}
              onValueChange={(v) =>
                setForm({ ...form, onConflict: v === 'error' ? null : (v as OnConflict) })
              }
              options={(['error', 'skip', 'update'] as const).map((c) => ({
                value: c,
                label: t(`import.conflict.${c}`),
              }))}
            />
          </Field>
        )}
        <Field label={t('import.mode')} htmlFor="import-mode">
          <Select
            id="import-mode"
            aria-label={t('import.mode')}
            value={form.mode}
            onValueChange={(v) => setForm({ ...form, mode: v as 'batches' | 'single' })}
            options={[
              { value: 'batches', label: t('import.modes.batches') },
              { value: 'single', label: t('import.modes.single') },
            ]}
          />
        </Field>
        {form.mode === 'batches' && (
          <Field label={t('import.batchRows')} htmlFor="import-batch">
            <Input
              id="import-batch"
              type="number"
              min={1}
              value={form.batchRows}
              onChange={(e) =>
                setForm({ ...form, batchRows: Math.max(1, Number(e.target.value) || 1) })
              }
            />
          </Field>
        )}
        <Field label={t('import.maxErrors')} htmlFor="import-max-errors">
          <Input
            id="import-max-errors"
            type="number"
            min={0}
            value={form.maxErrors}
            onChange={(e) =>
              setForm({ ...form, maxErrors: Math.max(0, Number(e.target.value) || 0) })
            }
          />
          <p className="text-xs text-muted-foreground">{t('import.maxErrorsHint')}</p>
        </Field>
      </div>
    </div>
  );
}

/** Production asks the user to type a text before a destructive import runs. */
function ConfirmPanel({
  request,
  typed,
  setTyped,
}: {
  request: ConfirmationRequest;
  typed: string;
  setTyped: (v: string) => void;
}) {
  const { t } = useTranslation('export');
  return (
    <div
      role="alert"
      data-testid="import-confirm"
      className="space-y-2 rounded-md border border-danger p-2.5 text-xs"
    >
      <p className="font-medium text-danger">{t('import.confirm.title')}</p>
      {request.statements.length === 0 ? (
        <p>{t('import.confirm.overwrite')}</p>
      ) : (
        <ul className="max-h-24 overflow-auto font-mono">
          {request.statements.slice(0, 5).map((s) => (
            <li key={s.index} className="truncate">
              {s.text}
            </li>
          ))}
        </ul>
      )}
      <Field
        label={t('import.confirm.type', { text: request.confirmText })}
        htmlFor="import-confirm-text"
      >
        <Input id="import-confirm-text" value={typed} onChange={(e) => setTyped(e.target.value)} />
      </Field>
    </div>
  );
}

/** Import wizard: file, preview, target, column mapping. Starts a background job. */
export function ImportWizard({ open, onOpenChange, session, target, parent }: ImportWizardProps) {
  const { t } = useTranslation('export');
  const [raw, setForm] = useState<ImportForm>(() => defaultImportForm(target));
  const [stepIndex, setStepIndex] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirm, setConfirm] = useState<ConfirmationRequest | null>(null);
  const [typed, setTyped] = useState('');
  const scope = useMemo(() => parent ?? (target ? parentOf(target) : null), [parent, target]);
  const steps = raw.kind === 'sql' ? (['file'] as const) : (['file', 'target', 'mapping'] as const);
  const step = steps[Math.min(stepIndex, steps.length - 1)] ?? 'file';
  const last = stepIndex >= steps.length - 1;

  const format = importFormat(raw);
  const preview = useQuery({
    queryKey: ['importPreview', raw.file, JSON.stringify(format)],
    enabled: raw.file !== '',
    queryFn: () => ipc.importPreview({ file: raw.file, format, rows: 50 }),
  });
  const data = preview.data;
  const details = useQuery({
    queryKey: ['importTable', session.sessionId, raw.table ? pathKey(raw.table) : ''],
    enabled: raw.targetMode === 'existing' && !!raw.table && raw.kind !== 'sql',
    queryFn: () => ipc.tableDetails(session.sessionId, raw.table as ObjectPath),
  });
  const tableColumns = useMemo(
    () => details.data?.columns.map((c) => c.name) ?? [],
    [details.data],
  );

  // Proposals that follow the preview: columns of a new table and the mapping onto the table.
  // They are derived while the user has not edited them (an edit stores the whole array).
  const form = useMemo<ImportForm>(() => {
    let next = raw;
    if (data && next.targetMode === 'new' && next.newColumns.length === 0) {
      next = { ...next, newColumns: newColumnsFromPreview(data, session.dialect) };
    }
    if (data && next.mapping.length === 0 && tableColumns.length > 0) {
      next = { ...next, mapping: autoMapping(data.columns, tableColumns) };
    }
    return next;
  }, [raw, data, tableColumns, session.dialect]);

  const issues = validateImport(form, scope);
  const nextBlocked =
    (step === 'file' && (form.file === '' || !data)) ||
    (step === 'target' &&
      (form.targetMode === 'existing' ? !form.table : form.newName.trim() === '' || !scope));
  const fileColumns = data?.columns ?? [];

  const start = async () => {
    setBusy(true);
    setError(null);
    try {
      const spec = buildImportSpec(session.connectionId, form, fileColumns, scope);
      await ipc.importStart(spec, confirm ? { token: confirm.token, typed } : null);
      toast.info(t('import.started'));
      useJobsStore.getState().setPanelOpen(true);
      onOpenChange(false);
    } catch (e) {
      const err = e as Partial<IpcError>;
      if (err.kind === 'confirmationRequired' && err.confirmation) {
        setConfirm(err.confirmation);
        setTyped('');
      } else {
        setConfirm(null);
        setError(ipcMessage(e));
      }
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        title={t('import.title')}
        description={t(`import.step.${step}`)}
        className="w-[min(640px,94vw)]"
        footer={
          <>
            <Button disabled={stepIndex === 0} onClick={() => setStepIndex(stepIndex - 1)}>
              <ChevronLeft size={13} />
              {t('back')}
            </Button>
            {last ? (
              <Button
                variant="primary"
                disabled={
                  busy || issues.length > 0 || (confirm !== null && typed !== confirm.confirmText)
                }
                onClick={() => void start()}
              >
                {confirm ? t('import.confirm.go') : t('import.start')}
              </Button>
            ) : (
              <Button
                variant="primary"
                disabled={nextBlocked}
                onClick={() => setStepIndex(stepIndex + 1)}
              >
                {t('next')}
                <ChevronRight size={13} />
              </Button>
            )}
          </>
        }
      >
        {step === 'file' && (
          <FileStep
            form={form}
            setForm={setForm}
            preview={data}
            loading={preview.isFetching}
            error={preview.isError ? ipcMessage(preview.error) : null}
          />
        )}
        {step === 'target' && (
          <TargetStep form={form} setForm={setForm} session={session} parent={scope} />
        )}
        {step === 'mapping' && data && (
          <MappingStep form={form} setForm={setForm} preview={data} tableColumns={tableColumns} />
        )}
        {confirm && <ConfirmPanel request={confirm} typed={typed} setTyped={setTyped} />}
        {error && (
          <p role="alert" className="mt-3 select-text break-words text-xs text-danger">
            {error}
          </p>
        )}
      </DialogContent>
    </Dialog>
  );
}

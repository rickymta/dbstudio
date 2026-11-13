import { CheckCircle2, TriangleAlert } from 'lucide-react';
import { lazy, Suspense, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Spinner } from '@/components/ui/Spinner';
import { ipc, toIpcError } from '@/lib/ipc';
import type { DocumentChange, IpcError, ObjectPath, Uuid, Value } from '@/lib/types';
import {
  buildDocumentChangeSet,
  deleteChange,
  insertChange,
  replaceChange,
} from './documentChanges';
import { toEjson, toEjsonCompact, validateEjson } from './ejson';
import { docId } from './flatten';

// CodeMirror is a separate chunk, fetched when the first editor opens.
const JsonEditor = lazy(() => import('./JsonEditor'));

const NEW_DOCUMENT = '{\n  \n}';

interface CommonProps {
  sessionId: Uuid;
  path: ObjectPath;
  /** The change was applied: reload the data. */
  onApplied: () => void;
  onClose: () => void;
}

interface DocumentEditorProps extends CommonProps {
  /** Document to edit; `null` adds a new one. */
  doc: Value | null;
  /** The server cannot apply a change set atomically (Mongo standalone): show a note. */
  atomic?: boolean;
}

function errorText(e: IpcError): string {
  return e.kind === 'changeFailed' && e.applied
    ? `${e.message} (${e.applied} applied before it)`
    : e.message;
}

function usePreview(sessionId: Uuid, path: ObjectPath, change: DocumentChange | null) {
  const [commands, setCommands] = useState<string[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const run = async () => {
    if (!change) return;
    setBusy(true);
    setError(null);
    try {
      setCommands(await ipc.changesPreview(sessionId, buildDocumentChangeSet(path, [change])));
    } catch (e) {
      setCommands(null);
      setError(toIpcError(e).message);
    } finally {
      setBusy(false);
    }
  };
  return { commands, error, busy, run, reset: () => setCommands(null) };
}

function Commands({ commands }: { commands: string[] }) {
  const { t } = useTranslation('document');
  return (
    <section aria-label={t('editor.previewTitle')} className="flex flex-col gap-1">
      <h4 className="text-xs font-medium text-muted-foreground">{t('editor.previewTitle')}</h4>
      <pre className="max-h-32 overflow-auto rounded-md bg-surface-muted p-2 font-mono text-xs break-all whitespace-pre-wrap">
        {commands.join('\n')}
      </pre>
    </section>
  );
}

/** Edit (replace) or add a document as Extended JSON, with live syntax checking and preview. */
export function DocumentEditor({
  sessionId,
  path,
  doc,
  atomic = true,
  onApplied,
  onClose,
}: DocumentEditorProps) {
  const { t } = useTranslation('document');
  const initial = useMemo(() => (doc ? toEjson(doc) : NEW_DOCUMENT), [doc]);
  const [text, setText] = useState(initial);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  const id = doc ? docId(doc) : undefined;
  const missingId = doc !== null && id === undefined;
  const check = useMemo(() => validateEjson(text), [text]);
  const change = useMemo<DocumentChange | null>(() => {
    if (!check.ok) return null;
    if (doc === null) return insertChange(text);
    return id ? replaceChange(id, text) : null;
  }, [check.ok, doc, id, text]);
  const preview = usePreview(sessionId, path, change);

  // A new text invalidates a shown preview.
  const { reset } = preview;
  useEffect(() => {
    reset();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [text]);

  const save = async () => {
    if (!change || saving) return;
    setSaving(true);
    setSaveError(null);
    try {
      await ipc.changesApply(sessionId, buildDocumentChangeSet(path, [change]));
      onApplied();
      onClose();
    } catch (e) {
      setSaveError(errorText(toIpcError(e)));
      setSaving(false);
    }
  };

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent
        title={doc ? t('editor.editTitle') : t('editor.insertTitle')}
        description={t('editor.description')}
        className="h-[80vh] w-[min(760px,94vw)]"
        footer={
          <>
            <Button
              className="mr-auto"
              onClick={() => void preview.run()}
              disabled={!change || preview.busy || saving}
            >
              {preview.busy && <Spinner size={12} />}
              {t('editor.preview')}
            </Button>
            <Button onClick={onClose}>{t('editor.cancel')}</Button>
            <Button variant="primary" onClick={() => void save()} disabled={!change || saving}>
              {saving && <Spinner size={12} className="text-accent-foreground" />}
              {saving ? t('editor.saving') : t('editor.save')}
            </Button>
          </>
        }
      >
        <div className="flex h-full min-h-0 flex-col gap-2">
          <div className="min-h-48 flex-1 overflow-hidden rounded-md border border-border">
            <Suspense
              fallback={
                <div className="flex h-full items-center justify-center">
                  <Spinner />
                </div>
              }
            >
              <JsonEditor
                value={initial}
                label={t('editor.label')}
                onChange={setText}
                onSubmit={() => void save()}
              />
            </Suspense>
          </div>
          {check.ok ? (
            <p role="status" className="flex items-center gap-1.5 text-xs text-success">
              <CheckCircle2 size={13} /> {t('editor.valid')}
            </p>
          ) : (
            <p role="alert" className="flex items-start gap-1.5 text-xs text-danger">
              <TriangleAlert size={13} className="mt-px shrink-0" />
              <span className="min-w-0 break-words">
                {t('editor.parseError', { line: check.line, col: check.col, message: check.error })}
              </span>
            </p>
          )}
          {missingId && (
            <p role="alert" className="text-xs text-danger">
              {t('editor.noId')}
            </p>
          )}
          {!atomic && <p className="text-xs text-muted-foreground">{t('editor.notAtomic')}</p>}
          {preview.commands && <Commands commands={preview.commands} />}
          {preview.error && (
            <p role="alert" className="text-xs break-words text-danger">
              {t('editor.previewFailed')}: {preview.error}
            </p>
          )}
          {saveError && (
            <p role="alert" className="text-xs break-words text-danger">
              {t('editor.saveFailed')}: {saveError}
            </p>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}

/** Confirmation for deleting one document; shows the command that will run. */
export function DeleteDocumentDialog({
  sessionId,
  path,
  doc,
  onApplied,
  onClose,
}: CommonProps & { doc: Value }) {
  const { t } = useTranslation('document');
  const id = docId(doc);
  const change = useMemo(() => (id ? deleteChange(id) : null), [id]);
  const preview = usePreview(sessionId, path, change);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const { run } = preview;
  useEffect(() => {
    void run();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const confirm = async () => {
    if (!change || busy) return;
    setBusy(true);
    setError(null);
    try {
      await ipc.changesApply(sessionId, buildDocumentChangeSet(path, [change]));
      onApplied();
      onClose();
    } catch (e) {
      setError(errorText(toIpcError(e)));
      setBusy(false);
    }
  };

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent
        title={t('delete.title')}
        description={t('delete.description')}
        className="w-[min(520px,94vw)]"
        footer={
          <>
            <Button onClick={onClose}>{t('editor.cancel')}</Button>
            <Button variant="danger" onClick={() => void confirm()} disabled={!change || busy}>
              {busy && <Spinner size={12} />}
              {t('delete.confirm')}
            </Button>
          </>
        }
      >
        <div className="flex flex-col gap-2">
          <code className="rounded-md bg-surface-muted p-2 font-mono text-xs break-all">
            {id ? toEjsonCompact(id) : ''}
          </code>
          {preview.commands && <Commands commands={preview.commands} />}
          {error && (
            <p role="alert" className="text-xs break-words text-danger">
              {t('delete.failed')}: {error}
            </p>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}

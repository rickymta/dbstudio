import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Checkbox } from '@/components/ui/Checkbox';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Textarea } from '@/components/ui/Textarea';
import type { ColumnMeta, EditValue, Value } from '@/lib/types';
import { editorInitialText, editorKind, isNullEdit, NULL_EDIT, parseInput } from './cellEdit';

interface CellEditorDialogProps {
  column: ColumnMeta;
  current: Value | EditValue | null;
  onCommit: (edit: EditValue) => void;
  onCancel: () => void;
}

function isNullCurrent(current: Value | EditValue | null): boolean {
  if (!current) return false;
  return 'kind' in current ? isNullEdit(current) : current.t === 'null';
}

/** Multi-line editor for JSON, long text and binary cells (binary can only become NULL). */
export function CellEditorDialog({ column, current, onCommit, onCancel }: CellEditorDialogProps) {
  const { t } = useTranslation('data');
  const kind = editorKind(column.valueKind);
  const [text, setText] = useState(() => editorInitialText(current));
  const [asNull, setAsNull] = useState(() => isNullCurrent(current));
  const result = parseInput(column.valueKind, text, asNull);
  const error = result.ok ? null : t(`edit.error.${result.error}`);
  const binary = kind === 'binary';

  const save = () => {
    if (binary) return onCommit(NULL_EDIT);
    if (result.ok) onCommit(result.edit);
  };

  return (
    <Dialog open onOpenChange={(open) => !open && onCancel()}>
      <DialogContent
        title={t('edit.editor.title', { column: column.name })}
        description={`${column.typeName}`}
        className="w-[min(640px,94vw)]"
        footer={
          <>
            <Button onClick={onCancel}>{t('common:actions.cancel')}</Button>
            <Button variant="primary" onClick={save} disabled={binary ? !asNull : !result.ok}>
              {t('edit.editor.save')}
            </Button>
          </>
        }
      >
        <div className="flex flex-col gap-2">
          {binary ? (
            <p className="text-xs text-muted-foreground">{t('edit.editor.binaryOnlyNull')}</p>
          ) : (
            <Textarea
              aria-label={t('edit.inputLabel', { column: column.name })}
              aria-invalid={error ? true : undefined}
              value={asNull ? '' : text}
              disabled={asNull}
              rows={12}
              spellCheck={false}
              className="font-mono text-xs"
              onChange={(e) => setText(e.target.value)}
            />
          )}
          <label className="flex items-center gap-2 text-xs">
            <Checkbox
              checked={asNull}
              onCheckedChange={(v) => setAsNull(v === true)}
              aria-label={t('edit.editor.nullLabel')}
            />
            <span>{t('edit.editor.nullLabel')}</span>
            <span className="text-muted-foreground">{t('edit.editor.nullHint')}</span>
          </label>
          {error && (
            <p role="alert" className="text-xs text-danger">
              {error}
            </p>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}

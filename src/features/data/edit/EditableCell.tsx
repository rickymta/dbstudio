import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { ColumnMeta, EditValue, Value } from '@/lib/types';
import { cn } from '@/lib/cn';
import { editorInitialText, editorKind, NULL_EDIT, parseInput } from './cellEdit';

/** Which option of the boolean select a cell starts on. */
function boolChoice(current: Value | EditValue | null): 'true' | 'false' | 'null' {
  const of = (v: Value) => (v.t === 'null' ? 'null' : v.t === 'bool' && !v.v ? 'false' : 'true');
  if (!current) return 'true';
  if ('kind' in current) return current.kind === 'value' ? of(current.v) : 'true';
  return of(current);
}

interface EditableCellProps {
  column: ColumnMeta;
  /** Current value of the cell: the fetched value or a pending edit. */
  current: Value | EditValue | null;
  /** The cell may be set to DEFAULT (new rows). */
  allowDefault: boolean;
  onCommit: (edit: EditValue) => void;
  onCancel: () => void;
}

/**
 * Inline editor for scalar cells. Enter or leaving the field commits, Escape cancels; the NULL
 * button writes NULL (an empty field writes the empty text, never NULL). Numbers and booleans are
 * validated before they are accepted.
 */
export function EditableCell({
  column,
  current,
  allowDefault,
  onCommit,
  onCancel,
}: EditableCellProps) {
  const { t } = useTranslation('data');
  const kind = editorKind(column.valueKind);
  const [initialText] = useState(() => editorInitialText(current));
  const [text, setText] = useState(initialText);
  const done = useRef(false);
  const inputRef = useRef<HTMLInputElement | HTMLSelectElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
    if (inputRef.current instanceof HTMLInputElement) inputRef.current.select();
  }, []);

  const result = parseInput(column.valueKind, text, false);
  const error = result.ok ? null : t(`edit.error.${result.error}`);

  const finish = (edit: EditValue | null) => {
    if (done.current) return;
    done.current = true;
    if (edit) onCommit(edit);
    else onCancel();
  };
  // Leaving the field without typing is not an edit (an untouched NULL must stay NULL).
  const commit = () => finish(result.ok && text !== initialText ? result.edit : null);

  if (kind === 'bool') {
    const initial = boolChoice(current);
    return (
      <select
        ref={inputRef as React.RefObject<HTMLSelectElement>}
        aria-label={t('edit.inputLabel', { column: column.name })}
        defaultValue={initial}
        className="h-full w-full bg-surface px-1 text-xs outline-2 -outline-offset-2 outline-accent"
        onChange={(e) => {
          const v = e.target.value;
          if (v === 'null') return finish(NULL_EDIT);
          finish({ kind: 'value', v: { t: 'bool', v: v === 'true' } });
        }}
        onKeyDown={(e) => {
          if (e.key === 'Escape') {
            e.stopPropagation();
            finish(null);
          }
        }}
        onBlur={() => finish(null)}
      >
        <option value="true">{t('edit.bool.true')}</option>
        <option value="false">{t('edit.bool.false')}</option>
        <option value="null">{t('grid.null')}</option>
      </select>
    );
  }

  return (
    <div
      className={cn(
        'flex h-full w-full items-stretch bg-surface outline-2 -outline-offset-2',
        error ? 'outline-danger' : 'outline-accent',
      )}
    >
      <input
        ref={inputRef as React.RefObject<HTMLInputElement>}
        aria-label={t('edit.inputLabel', { column: column.name })}
        aria-invalid={error ? true : undefined}
        title={error ?? undefined}
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          e.stopPropagation(); // the grid's navigation keys must not fire while typing
          if (e.key === 'Enter') {
            e.preventDefault();
            if (result.ok) commit();
            else if (text === initialText) finish(null);
          } else if (e.key === 'Escape') {
            finish(null);
          }
        }}
        onBlur={() => {
          if (!done.current) commit();
        }}
        className="min-w-0 flex-1 bg-transparent px-2 font-mono text-xs outline-none"
      />
      <button
        type="button"
        tabIndex={-1}
        title={t('edit.setNullTitle')}
        onMouseDown={(e) => {
          e.preventDefault(); // keep the input focused so blur does not commit the text
          finish(NULL_EDIT);
        }}
        className="shrink-0 border-l border-border px-1 text-[10px] text-muted-foreground hover:bg-border/50"
      >
        {t('edit.setNull')}
      </button>
      {allowDefault && (
        <button
          type="button"
          tabIndex={-1}
          title={t('edit.setDefaultTitle')}
          onMouseDown={(e) => {
            e.preventDefault();
            finish({ kind: 'default' });
          }}
          className="shrink-0 border-l border-border px-1 text-[10px] text-muted-foreground hover:bg-border/50"
        >
          {t('edit.setDefault')}
        </button>
      )}
    </div>
  );
}

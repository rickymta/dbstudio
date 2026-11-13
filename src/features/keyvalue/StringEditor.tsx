import { AlertTriangle } from 'lucide-react';
import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Textarea } from '@/components/ui/Textarea';
import { cn } from '@/lib/cn';
import type { KeyChange, Value } from '@/lib/types';
import { setStringChange } from './keyChanges';
import {
  convertDraft,
  draftToValue,
  initialStringMode,
  prettyJson,
  toDraft,
  valuesEqual,
  type StringMode,
} from './valueFormat';

interface StringEditorProps {
  keyName: string;
  value: Value;
  totalBytes: number;
  truncated: boolean;
  readOnly: boolean;
  stage: (change: KeyChange) => void;
}

const MODES: StringMode[] = ['text', 'json', 'hex'];

/** String value viewer/editor with Text, JSON (pretty) and Hex modes. */
export function StringEditor({
  keyName,
  value,
  totalBytes,
  truncated,
  readOnly,
  stage,
}: StringEditorProps) {
  const { t } = useTranslation('keyvalue');
  const [mode, setMode] = useState<StringMode>(() => initialStringMode(value));
  const [draft, setDraft] = useState(() => toDraft(value, initialStringMode(value)));
  // What the draft looked like before the user typed; a mode switch re-renders both.
  const [baseline, setBaseline] = useState(draft);
  const [modeError, setModeError] = useState(false);
  const editable = !readOnly && !truncated;

  const next = useMemo(() => draftToValue(draft, mode), [draft, mode]);
  const dirty = next !== null && draft !== baseline && !valuesEqual(next, value);
  const jsonInvalid = mode === 'json' && draft.trim() !== '' && prettyJson(draft) === null;

  const switchMode = (to: StringMode) => {
    const converted = convertDraft(draft, mode, to);
    if (converted === null) {
      setModeError(true);
      return;
    }
    setModeError(false);
    setBaseline(convertDraft(baseline, mode, to) ?? converted);
    setDraft(converted);
    setMode(to);
  };

  return (
    <div className="flex h-full min-h-0 flex-col gap-2 p-2">
      {truncated && (
        <p
          role="alert"
          className="flex items-center gap-1.5 rounded bg-warning-soft px-2 py-1 text-xs text-warning"
        >
          <AlertTriangle size={13} />{' '}
          {t('string.truncated', { shown: draft.length, total: totalBytes })}
        </p>
      )}
      <div className="flex items-center gap-1" role="group" aria-label={t('string.mode')}>
        {MODES.map((m) => (
          <Button
            key={m}
            size="sm"
            variant={mode === m ? 'primary' : 'outline'}
            aria-pressed={mode === m}
            onClick={() => switchMode(m)}
          >
            {t(`string.modes.${m}`)}
          </Button>
        ))}
        {modeError && (
          <span role="alert" className="ml-2 text-xs text-danger">
            {t('string.notConvertible')}
          </span>
        )}
        {jsonInvalid && (
          <span className="ml-2 text-xs text-warning">{t('string.invalidJson')}</span>
        )}
        <span className="ml-auto text-xs text-muted-foreground">
          {t('string.bytes', { count: totalBytes })}
        </span>
      </div>
      <Textarea
        aria-label={t('string.value')}
        value={draft}
        readOnly={!editable}
        spellCheck={false}
        onChange={(e) => setDraft(e.target.value)}
        className={cn('min-h-0 flex-1 resize-none', mode === 'hex' && 'break-all')}
      />
      {mode === 'hex' && next === null && (
        <span role="alert" className="text-xs text-danger">
          {t('string.invalidHex')}
        </span>
      )}
      {!readOnly && (
        <div className="flex items-center justify-end gap-2">
          <Button
            disabled={!dirty}
            onClick={() => {
              setDraft(baseline);
              setModeError(false);
            }}
          >
            {t('string.revert')}
          </Button>
          <Button
            variant="primary"
            disabled={!editable || !dirty || next === null}
            onClick={() => next && stage(setStringChange(keyName, next))}
          >
            {t('string.save')}
          </Button>
        </div>
      )}
    </div>
  );
}

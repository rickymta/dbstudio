import { Plus, Trash2 } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { IconButton } from '@/components/ui/IconButton';
import { Input } from '@/components/ui/Input';

type Row = { key: string; value: string };

const toRows = (o: Record<string, string>): Row[] =>
  Object.entries(o).map(([key, value]) => ({ key, value }));

/** Rows to the options record: blank keys are dropped, the last duplicate wins. */
function rowsToOptions(rows: Row[]): Record<string, string> {
  const out: Record<string, string> = {};
  for (const r of rows) if (r.key.trim() !== '') out[r.key.trim()] = r.value;
  return out;
}

const same = (a: Record<string, string>, b: Record<string, string>) =>
  JSON.stringify(Object.entries(a)) === JSON.stringify(Object.entries(b));

/** Editable key=value list of driver options (what a pasted URI carried beyond the basics). */
export function OptionsEditor({
  value,
  onChange,
}: {
  value: Record<string, string>;
  onChange: (options: Record<string, string>) => void;
}) {
  const { t } = useTranslation('document');
  const [rows, setRows] = useState<Row[]>(() => toRows(value));
  const emitted = useRef(value);

  // Options replaced from outside (a pasted URI) reset the rows; our own edits do not.
  useEffect(() => {
    if (!same(value, emitted.current)) {
      emitted.current = value;
      setRows(toRows(value));
    }
  }, [value]);

  const commit = (next: Row[]) => {
    setRows(next);
    const options = rowsToOptions(next);
    emitted.current = options;
    onChange(options);
  };

  return (
    <div className="flex flex-col gap-1.5">
      <p className="text-xs text-muted-foreground">{t('options.hint')}</p>
      {rows.map((r, i) => (
        <div key={i} className="flex items-center gap-1">
          <Input
            aria-label={t('options.key')}
            className="w-40 font-mono text-xs"
            spellCheck={false}
            value={r.key}
            onChange={(e) =>
              commit(rows.map((x, j) => (j === i ? { ...x, key: e.target.value } : x)))
            }
          />
          <span className="text-muted-foreground">=</span>
          <Input
            aria-label={t('options.value')}
            className="min-w-0 flex-1 font-mono text-xs"
            spellCheck={false}
            value={r.value}
            onChange={(e) =>
              commit(rows.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)))
            }
          />
          <IconButton
            type="button"
            label={t('options.remove')}
            icon={<Trash2 size={13} />}
            onClick={() => commit(rows.filter((_, j) => j !== i))}
          />
        </div>
      ))}
      <Button
        type="button"
        className="self-start"
        onClick={() => setRows((r) => [...r, { key: '', value: '' }])}
      >
        <Plus size={13} />
        {t('options.add')}
      </Button>
    </div>
  );
}

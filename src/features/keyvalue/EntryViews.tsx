import { Plus, Trash2 } from 'lucide-react';
import { useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Input } from '@/components/ui/Input';
import { Textarea } from '@/components/ui/Textarea';
import type { KeyChange, KeyEntries, Value } from '@/lib/types';
import {
  hashDeleteChange,
  hashSetChange,
  listPushChange,
  listRemoveChange,
  listSetChange,
  parseFieldLines,
  setAddChange,
  setRemoveChange,
  streamAddChange,
  streamDeleteChange,
  zAddChange,
  zRemoveChange,
} from './keyChanges';
import { editText, editedValue, textValue, valueCell } from './valueFormat';

export interface EntryViewProps {
  keyName: string;
  readOnly: boolean;
  /** Queue a change in the pending list. */
  stage: (change: KeyChange) => void;
}

const th = 'sticky top-0 bg-surface-muted px-2 py-1 text-left text-xs font-medium';
const td = 'border-t border-border px-2 py-1 align-top font-mono text-[12px]';

/** A cell that turns into an input on double click; Enter commits, Escape cancels. */
function EditableCell({
  value,
  readOnly,
  onCommit,
  numeric = false,
}: {
  value: Value;
  readOnly: boolean;
  onCommit: (draft: string) => boolean;
  numeric?: boolean;
}) {
  const { t } = useTranslation('keyvalue');
  const [draft, setDraft] = useState<string | null>(null);
  const [invalid, setInvalid] = useState(false);
  if (draft === null) {
    return (
      <span
        className={readOnly ? undefined : 'cursor-text'}
        title={readOnly ? undefined : t('edit.hint')}
        onDoubleClick={() => {
          if (!readOnly) setDraft(editText(value));
        }}
      >
        {valueCell(value) || <span className="text-muted-foreground">{t('emptyValue')}</span>}
      </span>
    );
  }
  const commit = () => {
    if (onCommit(draft)) {
      setDraft(null);
      setInvalid(false);
    } else setInvalid(true);
  };
  return (
    <Input
      autoFocus
      aria-label={t('edit.cell')}
      aria-invalid={invalid}
      inputMode={numeric ? 'decimal' : undefined}
      value={draft}
      className="font-mono"
      onChange={(e) => setDraft(e.target.value)}
      onBlur={() => setDraft(null)}
      onKeyDown={(e) => {
        if (e.key === 'Enter') commit();
        if (e.key === 'Escape') setDraft(null);
      }}
    />
  );
}

function DeleteButton({ onClick, disabled }: { onClick: () => void; disabled: boolean }) {
  const { t } = useTranslation('keyvalue');
  return (
    <Button
      size="icon"
      variant="ghost"
      aria-label={t('actions.delete')}
      title={t('actions.delete')}
      onClick={onClick}
      disabled={disabled}
    >
      <Trash2 size={13} />
    </Button>
  );
}

function AddRow({ children }: { children: ReactNode }) {
  return (
    <div className="flex items-center gap-2 border-b border-border px-2 py-1.5">{children}</div>
  );
}

function Empty() {
  const { t } = useTranslation('keyvalue');
  return <p className="p-3 text-xs italic text-muted-foreground">{t('noEntries')}</p>;
}

type Hash = Extract<KeyEntries, { type: 'hash' }>;

export function HashView({
  entries,
  keyName,
  readOnly,
  stage,
}: EntryViewProps & { entries: Hash }) {
  const { t } = useTranslation('keyvalue');
  const [field, setField] = useState('');
  const [value, setValue] = useState('');
  return (
    <div>
      {!readOnly && (
        <AddRow>
          <Input
            aria-label={t('hash.newField')}
            placeholder={t('hash.field')}
            value={field}
            onChange={(e) => setField(e.target.value)}
          />
          <Input
            aria-label={t('hash.newValue')}
            placeholder={t('hash.value')}
            value={value}
            onChange={(e) => setValue(e.target.value)}
          />
          <Button
            disabled={field === ''}
            onClick={() => {
              stage(hashSetChange(keyName, textValue(field), textValue(value)));
              setField('');
              setValue('');
            }}
          >
            <Plus size={13} /> {t('hash.add')}
          </Button>
        </AddRow>
      )}
      {entries.fields.length === 0 ? (
        <Empty />
      ) : (
        <table className="w-full border-collapse text-[13px]">
          <thead>
            <tr>
              <th className={th}>{t('hash.field')}</th>
              <th className={th}>{t('hash.value')}</th>
              <th className={th} />
            </tr>
          </thead>
          <tbody>
            {entries.fields.map(([f, v], i) => (
              <tr key={`${i}:${editText(f)}`}>
                <td className={td}>{valueCell(f)}</td>
                <td className={td}>
                  <EditableCell
                    value={v}
                    readOnly={readOnly}
                    onCommit={(d) => {
                      const next = editedValue(v, d);
                      if (!next) return false;
                      stage(hashSetChange(keyName, f, next));
                      return true;
                    }}
                  />
                </td>
                <td className={`${td} w-8`}>
                  <DeleteButton
                    disabled={readOnly}
                    onClick={() => stage(hashDeleteChange(keyName, f))}
                  />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

type List = Extract<KeyEntries, { type: 'list' }>;

export function ListView({
  entries,
  keyName,
  readOnly,
  stage,
}: EntryViewProps & { entries: List }) {
  const { t } = useTranslation('keyvalue');
  const [value, setValue] = useState('');
  return (
    <div>
      {!readOnly && (
        <AddRow>
          <Input
            aria-label={t('list.newValue')}
            placeholder={t('list.value')}
            value={value}
            onChange={(e) => setValue(e.target.value)}
          />
          {([true, false] as const).map((head) => (
            <Button
              key={String(head)}
              disabled={value === ''}
              onClick={() => {
                stage(listPushChange(keyName, textValue(value), head));
                setValue('');
              }}
            >
              <Plus size={13} /> {head ? t('list.pushHead') : t('list.pushTail')}
            </Button>
          ))}
        </AddRow>
      )}
      {entries.items.length === 0 ? (
        <Empty />
      ) : (
        <table className="w-full border-collapse text-[13px]">
          <thead>
            <tr>
              <th className={`${th} w-20`}>{t('list.index')}</th>
              <th className={th}>{t('list.value')}</th>
              <th className={th} />
            </tr>
          </thead>
          <tbody>
            {entries.items.map((v, i) => {
              const index = entries.start + i;
              return (
                <tr key={index}>
                  <td className={`${td} text-muted-foreground`}>{index}</td>
                  <td className={td}>
                    <EditableCell
                      value={v}
                      readOnly={readOnly}
                      onCommit={(d) => {
                        const next = editedValue(v, d);
                        if (!next) return false;
                        stage(listSetChange(keyName, index, next, v));
                        return true;
                      }}
                    />
                  </td>
                  <td className={`${td} w-8`}>
                    <DeleteButton
                      disabled={readOnly}
                      onClick={() => stage(listRemoveChange(keyName, index, v))}
                    />
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
    </div>
  );
}

type SetEntries = Extract<KeyEntries, { type: 'set' }>;

export function SetView({
  entries,
  keyName,
  readOnly,
  stage,
}: EntryViewProps & { entries: SetEntries }) {
  const { t } = useTranslation('keyvalue');
  const [member, setMember] = useState('');
  return (
    <div>
      {!readOnly && (
        <AddRow>
          <Input
            aria-label={t('set.newMember')}
            placeholder={t('set.member')}
            value={member}
            onChange={(e) => setMember(e.target.value)}
          />
          <Button
            disabled={member === ''}
            onClick={() => {
              stage(setAddChange(keyName, textValue(member)));
              setMember('');
            }}
          >
            <Plus size={13} /> {t('set.add')}
          </Button>
        </AddRow>
      )}
      {entries.members.length === 0 ? (
        <Empty />
      ) : (
        <table className="w-full border-collapse text-[13px]">
          <thead>
            <tr>
              <th className={th}>{t('set.member')}</th>
              <th className={th} />
            </tr>
          </thead>
          <tbody>
            {entries.members.map((m, i) => (
              <tr key={`${i}:${editText(m)}`}>
                <td className={td}>{valueCell(m)}</td>
                <td className={`${td} w-8`}>
                  <DeleteButton
                    disabled={readOnly}
                    onClick={() => stage(setRemoveChange(keyName, m))}
                  />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

type ZSet = Extract<KeyEntries, { type: 'zSet' }>;

export function ZSetView({
  entries,
  keyName,
  readOnly,
  stage,
}: EntryViewProps & { entries: ZSet }) {
  const { t } = useTranslation('keyvalue');
  const [member, setMember] = useState('');
  const [score, setScore] = useState('');
  const validScore = (s: string) =>
    s.trim() !== '' && !Number.isNaN(Number(s.trim().replace(/^[+-]?inf$/i, 'Infinity')));
  return (
    <div>
      {!readOnly && (
        <AddRow>
          <Input
            aria-label={t('zset.newMember')}
            placeholder={t('zset.member')}
            value={member}
            onChange={(e) => setMember(e.target.value)}
          />
          <Input
            aria-label={t('zset.newScore')}
            placeholder={t('zset.score')}
            value={score}
            className="w-32"
            onChange={(e) => setScore(e.target.value)}
          />
          <Button
            disabled={member === '' || !validScore(score)}
            onClick={() => {
              stage(zAddChange(keyName, textValue(member), score));
              setMember('');
              setScore('');
            }}
          >
            <Plus size={13} /> {t('zset.add')}
          </Button>
        </AddRow>
      )}
      {entries.members.length === 0 ? (
        <Empty />
      ) : (
        <table className="w-full border-collapse text-[13px]">
          <thead>
            <tr>
              <th className={th}>{t('zset.member')}</th>
              <th className={`${th} w-40`}>{t('zset.score')}</th>
              <th className={th} />
            </tr>
          </thead>
          <tbody>
            {entries.members.map(([m, s], i) => (
              <tr key={`${i}:${editText(m)}`}>
                <td className={td}>{valueCell(m)}</td>
                <td className={td}>
                  <EditableCell
                    numeric
                    value={textValue(s)}
                    readOnly={readOnly}
                    onCommit={(d) => {
                      if (!validScore(d)) return false;
                      stage(zAddChange(keyName, m, d));
                      return true;
                    }}
                  />
                </td>
                <td className={`${td} w-8`}>
                  <DeleteButton
                    disabled={readOnly}
                    onClick={() => stage(zRemoveChange(keyName, m))}
                  />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

type Stream = Extract<KeyEntries, { type: 'stream' }>;

export function StreamView({
  entries,
  keyName,
  readOnly,
  stage,
}: EntryViewProps & { entries: Stream }) {
  const { t } = useTranslation('keyvalue');
  const [id, setId] = useState('');
  const [fields, setFields] = useState('');
  const parsed = parseFieldLines(fields);
  return (
    <div>
      <p className="border-b border-border px-2 py-1 text-xs text-muted-foreground">
        {t('stream.groups', { count: entries.groups })}
      </p>
      {!readOnly && (
        <AddRow>
          <Input
            aria-label={t('stream.newId')}
            placeholder={t('stream.idAuto')}
            value={id}
            className="w-48"
            onChange={(e) => setId(e.target.value)}
          />
          <Textarea
            aria-label={t('stream.newFields')}
            placeholder={t('stream.fieldsHint')}
            rows={2}
            value={fields}
            onChange={(e) => setFields(e.target.value)}
          />
          <Button
            disabled={!parsed}
            onClick={() => {
              if (!parsed) return;
              stage(streamAddChange(keyName, id, parsed));
              setId('');
              setFields('');
            }}
          >
            <Plus size={13} /> {t('stream.add')}
          </Button>
        </AddRow>
      )}
      {entries.entries.length === 0 ? (
        <Empty />
      ) : (
        <table className="w-full border-collapse text-[13px]">
          <thead>
            <tr>
              <th className={`${th} w-56`}>{t('stream.id')}</th>
              <th className={th}>{t('stream.fields')}</th>
              <th className={th} />
            </tr>
          </thead>
          <tbody>
            {entries.entries.map((e) => (
              <tr key={e.id}>
                <td className={td}>{e.id}</td>
                <td className={td}>
                  {e.fields.map(([f, v]) => `${valueCell(f, 60)}=${valueCell(v, 120)}`).join('  ')}
                </td>
                <td className={`${td} w-8`}>
                  <DeleteButton
                    disabled={readOnly}
                    onClick={() => stage(streamDeleteChange(keyName, e.id))}
                  />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

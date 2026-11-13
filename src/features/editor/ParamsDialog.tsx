// Parameter values before a run (QUERY-10): `:name`, `$1`, `?`, `@p`.

import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';
import { Select } from '@/components/ui/Select';
import type { ParamRef } from '@/lib/types';
import { PARAM_KINDS, draftError, draftsFor, type ParamDraft, type ParamKind } from './params';

export interface ParamsDialogProps {
  open: boolean;
  refs: ParamRef[];
  /** Values typed the last time (kept for the same parameter names). */
  previous: ParamDraft[];
  onSubmit: (drafts: ParamDraft[]) => void;
  onCancel: () => void;
}

export function ParamsDialog(props: ParamsDialogProps) {
  // Mounted only while open, so every run starts from the remembered values.
  return props.open ? <ParamsForm {...props} /> : null;
}

function ParamsForm({ refs, previous, onSubmit, onCancel }: ParamsDialogProps) {
  const { t } = useTranslation('editor');
  const [drafts, setDrafts] = useState<ParamDraft[]>(() => draftsFor(refs, previous));

  const errors = drafts.map(draftError);
  const valid = errors.every((e) => e === null);
  const patch = (i: number, p: Partial<ParamDraft>) =>
    setDrafts((ds) => ds.map((d, j) => (j === i ? { ...d, ...p } : d)));
  const submit = () => valid && onSubmit(drafts);

  return (
    <Dialog open onOpenChange={(o) => !o && onCancel()}>
      <DialogContent
        title={t('params.title')}
        description={t('params.description')}
        className="w-[min(560px,94vw)]"
        footer={
          <>
            <Button onClick={onCancel}>{t('params.cancel')}</Button>
            <Button variant="primary" disabled={!valid} onClick={submit}>
              {t('params.run')}
            </Button>
          </>
        }
      >
        <form
          className="flex flex-col gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          {drafts.map((d, i) => (
            <div key={d.name} className="grid grid-cols-[7rem_8rem_1fr] items-start gap-2">
              <code className="pt-1 text-xs break-all">{d.name}</code>
              <Select
                aria-label={t('params.typeOf', { name: d.name })}
                value={d.kind}
                onValueChange={(k) =>
                  patch(i, { kind: k as ParamKind, value: k === 'bool' ? 'true' : d.value })
                }
                options={PARAM_KINDS.map((k) => ({ value: k, label: t(`params.kind.${k}`) }))}
              />
              <div className="flex flex-col gap-0.5">
                {d.kind === 'null' ? (
                  <span className="pt-1 text-xs text-muted-foreground">NULL</span>
                ) : d.kind === 'bool' ? (
                  <Select
                    aria-label={t('params.valueOf', { name: d.name })}
                    value={d.value === 'false' ? 'false' : 'true'}
                    onValueChange={(v) => patch(i, { value: v })}
                    options={[
                      { value: 'true', label: 'true' },
                      { value: 'false', label: 'false' },
                    ]}
                  />
                ) : (
                  <Input
                    aria-label={t('params.valueOf', { name: d.name })}
                    aria-invalid={errors[i] !== null}
                    autoFocus={i === 0}
                    spellCheck={false}
                    value={d.value}
                    onChange={(e) => patch(i, { value: e.target.value })}
                  />
                )}
                {errors[i] && <span className="text-[11px] text-danger">{t(errors[i])}</span>}
              </div>
            </div>
          ))}
          {/* Lets Enter submit from any text field. */}
          <button type="submit" hidden />
        </form>
      </DialogContent>
    </Dialog>
  );
}

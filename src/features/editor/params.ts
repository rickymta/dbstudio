// Query parameter drafts (QUERY-10): what the params dialog edits and how it becomes `ParamValue`s.

import type { ParamInput, ParamRef, ParamValue } from '@/lib/types';

export type ParamKind = ParamInput['kind'];

export const PARAM_KINDS: readonly ParamKind[] = ['text', 'number', 'bool', 'null', 'raw'];

export interface ParamDraft {
  name: string;
  kind: ParamKind;
  /** Text typed by the user; `'true'` / `'false'` for booleans. */
  value: string;
}

/** Same pattern the backend validates `Number` parameters against. */
export const NUMBER_PATTERN = /^-?\d+(\.\d+)?([eE][-+]?\d+)?$/;

/** Drafts for `refs`, keeping what the user entered before for the same names. */
export function draftsFor(refs: ParamRef[], previous: readonly ParamDraft[] = []): ParamDraft[] {
  return refs.map((r) => {
    const old = previous.find((d) => d.name === r.name);
    return old ? { ...old } : { name: r.name, kind: 'text', value: '' };
  });
}

/** `null` when the draft is valid, else the message key (namespace `editor`). */
export function draftError(d: ParamDraft): 'params.numberInvalid' | 'params.rawEmpty' | null {
  if (d.kind === 'number' && !NUMBER_PATTERN.test(d.value.trim())) return 'params.numberInvalid';
  if (d.kind === 'raw' && d.value.trim() === '') return 'params.rawEmpty';
  return null;
}

export function toParamValue(d: ParamDraft): ParamValue {
  switch (d.kind) {
    case 'null':
      return { name: d.name, value: { kind: 'null' } };
    case 'bool':
      return { name: d.name, value: { kind: 'bool', v: d.value === 'true' } };
    case 'number':
      return { name: d.name, value: { kind: 'number', v: d.value.trim() } };
    case 'raw':
      return { name: d.name, value: { kind: 'raw', v: d.value } };
    default:
      return { name: d.name, value: { kind: 'text', v: d.value } };
  }
}

export function toParamValues(drafts: readonly ParamDraft[]): ParamValue[] {
  return drafts.map(toParamValue);
}

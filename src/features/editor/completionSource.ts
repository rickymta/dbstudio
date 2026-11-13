// CodeMirror wiring of the pure completion logic.

import type { CompletionContext, CompletionSource } from '@codemirror/autocomplete';
import type { Uuid } from '@/lib/types';
import type { EditorMode } from '@/stores/tabs';
import { getSessionCatalog, getVocabulary, sqlVocabulary } from './catalogLoader';
import {
  completionsAt,
  consoleCompletionsAt,
  referencedTables,
  type CompletionOption,
} from './completion';

export interface CompletionTarget {
  sessionId: Uuid | null;
  mode: EditorMode;
  bracketQuotes: boolean;
  database: string | null;
  schema: string | null;
}

/** `target` is read on every call, so tab, session and context changes need no new source. */
export function createCompletionSource(getTarget: () => CompletionTarget): CompletionSource {
  return async (cm: CompletionContext) => {
    const target = getTarget();
    if (!target.sessionId) return null;
    const text = cm.state.doc.toString();
    const pos = cm.pos;
    const prefixStart = cm.matchBefore(/[\w$]*/)?.from ?? pos;
    const afterDot = prefixStart > 0 && text[prefixStart - 1] === '.';
    // Typing a word completes; an empty position only on request or right after a dot.
    if (!cm.explicit && prefixStart === pos && !afterDot && text[pos - 1] !== '$') return null;

    const vocab = await getVocabulary(target.sessionId);
    const catalog = await getSessionCatalog(target.sessionId, target.database, target.schema);
    if (cm.aborted) return null;

    let result;
    if (target.mode === 'sql') {
      await catalog.hydrate(referencedTables(text, pos, target.bracketQuotes));
      result = completionsAt({
        text,
        pos,
        catalog: catalog.catalog,
        vocabulary: sqlVocabulary(vocab),
        bracketQuotes: target.bracketQuotes,
      });
    } else {
      result = consoleCompletionsAt({
        mode: target.mode,
        text,
        pos,
        commands: vocab.commands,
        collections: catalog.catalog.tables.map((t) => t.name),
      });
    }
    if (!result || result.options.length === 0) return null;
    return {
      from: result.from,
      options: result.options.map(toCmOption),
      validFor: /^[\w$"]*$/,
    };
  };
}

function toCmOption(o: CompletionOption) {
  return {
    label: o.label,
    type: o.type === 'alias' ? 'variable' : o.type === 'column' ? 'property' : o.type,
    detail: o.detail,
    apply: o.apply,
    boost: o.boost,
  };
}

// Plug-in points of the console mode (QUERY-14, QUERY-15). The editor provides the frame:
// scrollback, input line, history, command hints (from `script_vocabulary`) and execution
// through the tab's lease. Feature packages add what is specific to their database:
//
// - Redis (package L): typed replies drawn like `redis-cli` (`keyvalue/ReplyView`); the command
//   vocabulary itself comes from the backend (`script_vocabulary` -> `commands`) and feeds the
//   console hints, so nothing to register for those.
// - MongoDB (package K): documents as Extended JSON (`document/ResultDocuments`), offered next
//   to the grid by `ResultsPanel`; collection names and operators already come from the catalog
//   and `completion.ts`.
//
// Both are registered below; a feature package can still replace one with `registerConsolePlugin`.

import type { ReactNode } from 'react';
import { renderMongoResult } from '@/features/document/renderResult';
import { renderRedisReply } from '@/features/keyvalue/renderReply';
import type { EditorMode } from '@/stores/tabs';
import type { StatementResult } from './results';

export interface ConsolePlugin {
  mode: EditorMode;
  /** Render one result of a command; return `null` to use the default text rendering. */
  renderResult?: (result: StatementResult) => ReactNode | null;
}

const plugins = new Map<EditorMode, ConsolePlugin>();

export function registerConsolePlugin(plugin: ConsolePlugin): () => void {
  plugins.set(plugin.mode, plugin);
  return () => {
    if (plugins.get(plugin.mode) === plugin) plugins.delete(plugin.mode);
  };
}

export const getConsolePlugin = (mode: EditorMode): ConsolePlugin | undefined => plugins.get(mode);

registerConsolePlugin({ mode: 'redis', renderResult: renderRedisReply });
registerConsolePlugin({ mode: 'mongo', renderResult: renderMongoResult });

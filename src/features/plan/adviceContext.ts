// Context shared by the diagram, the table and the advice panel: which nodes carry advice and
// the "look here" request raised by a click on an advice card.

import { createContext, useContext } from 'react';

const NONE: ReadonlySet<number> = new Set();

/** Ids of the nodes that carry advice. */
export const AdviceNodesContext = createContext<ReadonlySet<number>>(NONE);

/** A request to bring nodes into view; `seq` changes on every request, also for the same nodes. */
export interface AdviceFocus {
  seq: number;
  nodeIds: readonly number[];
}

export const AdviceFocusContext = createContext<AdviceFocus | null>(null);

export function useAdviceFocus(): AdviceFocus | null {
  return useContext(AdviceFocusContext);
}

/** Flash the outline of an element for 1.5 s (no-op where the Web Animations API is missing). */
export function flashElement(el: Element | null | undefined): void {
  if (!el || typeof (el as HTMLElement).animate !== 'function') return;
  (el as HTMLElement).animate(
    [
      { outline: '2px solid var(--accent)', outlineOffset: '2px' },
      { outline: '2px solid transparent', outlineOffset: '2px' },
      { outline: '2px solid var(--accent)', outlineOffset: '2px' },
      { outline: '2px solid transparent', outlineOffset: '2px' },
    ],
    { duration: 1500 },
  );
}

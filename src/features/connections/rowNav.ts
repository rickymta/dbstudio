import type { KeyboardEvent } from 'react';

/** Up/down moves between rows marked `data-nav-row` inside the sidebar. */
export function moveFocus(e: KeyboardEvent<HTMLElement>, delta: number) {
  const root = e.currentTarget.closest('[data-nav-root]');
  if (!root) return;
  const rows = Array.from(root.querySelectorAll<HTMLElement>('[data-nav-row]')).filter(
    (r) => !r.closest('[hidden]'),
  );
  const next = rows[rows.indexOf(e.currentTarget) + delta];
  if (next) {
    e.preventDefault();
    next.focus();
  } else if (delta < 0) {
    // Up from the first row returns to the search box.
    const search = root.querySelector<HTMLElement>('[data-search-input]');
    if (search) {
      e.preventDefault();
      search.focus();
    }
  }
}

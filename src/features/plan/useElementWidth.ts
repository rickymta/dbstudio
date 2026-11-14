import { useEffect, useState, type RefObject } from 'react';

/**
 * Width in px of the element behind `ref`, kept up to date; `null` until it has a real size
 * (no layout yet, or a test environment), which callers treat as "plenty of room".
 */
export function useElementWidth(ref: RefObject<HTMLElement | null>): number | null {
  const [width, setWidth] = useState<number | null>(null);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const measure = (w: number) => setWidth(w > 0 ? Math.round(w) : null);
    measure(el.getBoundingClientRect().width);
    if (typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver((entries) => {
      const last = entries[entries.length - 1];
      if (last) measure(last.contentRect.width);
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, [ref]);
  return width;
}

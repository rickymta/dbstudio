import { useEffect, useState } from 'react';

/** How long an open/test/save may take before the user is told what it may be waiting for. */
export const SLOW_CONNECT_MS = 3000;

/** True once `active` has been continuously true for `ms`. */
export function useSlowAfter(active: boolean, ms: number = SLOW_CONNECT_MS): boolean {
  const [slow, setSlow] = useState(false);
  useEffect(() => {
    if (!active) return;
    const timer = setTimeout(() => setSlow(true), ms);
    return () => {
      clearTimeout(timer);
      setSlow(false);
    };
  }, [active, ms]);
  return slow;
}

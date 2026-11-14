import { lazy, Suspense, type FC } from 'react';
import type { DebugSourcePayload } from '@/stores/tabs';

const DebugSourceTab = lazy(() => import('./DebugSourceTab'));

/** `debugSource` tab renderer registered in `app/tabRegistry.tsx`; loads its chunk on first use. */
export const LazyDebugSourceTab: FC<DebugSourcePayload> = (props) => (
  <Suspense fallback={null}>
    <DebugSourceTab {...props} />
  </Suspense>
);

import type { FC, ReactNode } from 'react';

export interface DebugBottomHostProps {
  tabStateId: string;
  /** The results panel the tool window replaces or sits beside while a session is live. */
  children: ReactNode;
}

/**
 * Bottom area of an editor tab: the Debug tool window while a session is live, otherwise just
 * the results. Contract placeholder: package F fills it.
 */
export const DebugBottomHost: FC<DebugBottomHostProps> = ({ children }) => <>{children}</>;

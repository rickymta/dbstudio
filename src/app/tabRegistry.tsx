import type { ReactNode } from 'react';
import { DdlTab } from '@/features/data/DdlTab';
import { TableTab } from '@/features/data/TableTab';
import { WelcomeTab } from '@/features/welcome/WelcomeTab';
import type { TabItem, TabKind } from '@/stores/tabs';

type Renderers = { [K in TabKind]: (tab: Extract<TabItem, { kind: K }>) => ReactNode };

/** Maps a tab kind to the component rendering it. Register new kinds here. */
const registry: Renderers = {
  welcome: () => <WelcomeTab />,
  table: (tab) => <TableTab {...tab.payload} />,
  ddl: (tab) => <DdlTab {...tab.payload} />,
};

export function renderTab(tab: TabItem): ReactNode {
  return (registry[tab.kind] as (t: TabItem) => ReactNode)(tab);
}

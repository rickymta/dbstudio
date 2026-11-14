import type { ReactNode } from 'react';
import { BackupTab } from '@/features/backup/BackupTab';
import { DdlTab } from '@/features/data/DdlTab';
import { LazyDebugSourceTab } from '@/features/debugger/LazyDebugSourceTab';
import { EditorTab } from '@/features/editor/EditorTab';
import { ErdTab } from '@/features/erd/ErdTab';
import { KeyTab } from '@/features/keyvalue/KeyTab';
import { PlanTab } from '@/features/plan/PlanTab';
import { WelcomeTab } from '@/features/welcome/WelcomeTab';
import type { TabItem, TabKind } from '@/stores/tabs';
import { ObjectDataTab } from './ObjectDataTab';

type Renderers = { [K in TabKind]: (tab: Extract<TabItem, { kind: K }>) => ReactNode };

/** Maps a tab kind to the component rendering it. Register new kinds here. */
const registry: Renderers = {
  welcome: () => <WelcomeTab />,
  table: (tab) => <ObjectDataTab {...tab.payload} />,
  ddl: (tab) => <DdlTab {...tab.payload} />,
  editor: (tab) => <EditorTab {...tab.payload} />,
  plan: (tab) => <PlanTab {...tab.payload} />,
  erd: (tab) => <ErdTab {...tab.payload} />,
  key: (tab) => <KeyTab {...tab.payload} />,
  debugSource: (tab) => <LazyDebugSourceTab {...tab.payload} />,
  backup: () => <BackupTab />,
};

export function renderTab(tab: TabItem): ReactNode {
  return (registry[tab.kind] as (t: TabItem) => ReactNode)(tab);
}

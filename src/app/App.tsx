import { Group, Panel, Separator } from 'react-resizable-panels';
import { ConnectionsSidebar } from '@/features/connections/ConnectionsSidebar';
import { TabBar } from '@/features/tabs/TabBar';
import { WelcomeTab } from '@/features/welcome/WelcomeTab';
import { useTabsStore } from '@/stores/tabs';
import { StatusBar } from './StatusBar';

function MainArea() {
  const activeId = useTabsStore((s) => s.activeId);
  return (
    <div className="flex h-full min-w-0 flex-col bg-background">
      <TabBar />
      <main className="min-h-0 flex-1" role="tabpanel">
        {activeId === 'welcome' && <WelcomeTab />}
      </main>
    </div>
  );
}

export function App() {
  return (
    <div className="flex h-full flex-col">
      <div className="min-h-0 flex-1">
        <Group orientation="horizontal">
          <Panel defaultSize="22%" minSize="180px" maxSize="50%">
            <ConnectionsSidebar />
          </Panel>
          <Separator className="w-px bg-border transition-colors hover:bg-accent data-[separator=active]:bg-accent" />
          <Panel minSize="40%">
            <MainArea />
          </Panel>
        </Group>
      </div>
      <StatusBar />
    </div>
  );
}

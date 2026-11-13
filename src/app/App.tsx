import { Group, Panel, Separator } from 'react-resizable-panels';
import { ConnectionsSidebar } from '@/features/connections/ConnectionsSidebar';
import { EditorHost } from '@/features/editor/EditorHost';
import { ConfirmDestructiveHost } from '@/features/safety/ConfirmDestructiveHost';
import { TabBar } from '@/features/tabs/TabBar';
import { useTabsStore } from '@/stores/tabs';
import { Toaster } from '@/components/ui/Toast';
import { StatusBar } from './StatusBar';
import { renderTab } from './tabRegistry';

function MainArea() {
  const activeTab = useTabsStore((s) => s.tabs.find((t) => t.id === s.activeId));
  return (
    <div className="flex h-full min-w-0 flex-col bg-background">
      <TabBar />
      <main className="min-h-0 flex-1" role="tabpanel">
        {activeTab && (
          <div key={activeTab.id} className="h-full">
            {renderTab(activeTab)}
          </div>
        )}
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
      <Toaster />
      {/* App-wide hosts, mounted once: editor shortcuts/dialogs and the destructive confirmation. */}
      <EditorHost />
      <ConfirmDestructiveHost />
    </div>
  );
}

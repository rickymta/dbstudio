import { useEffect } from 'react';
import { useWorkspaceStore, useSavedLayout } from '@/features/workspace/layout';
import { Group, Panel, Separator, usePanelRef } from 'react-resizable-panels';
import { ConnectionsSidebar } from '@/features/connections/ConnectionsSidebar';
import { SessionHealthBanner } from '@/features/connections/SessionHealthBanner';
import { EditorHost } from '@/features/editor/EditorHost';
import { ConfirmDestructiveHost } from '@/features/safety/ConfirmDestructiveHost';
import { TabBar } from '@/features/tabs/TabBar';
import { AppWindows } from '@/features/workspace/AppWindows';
import { CommandPalette } from '@/features/workspace/CommandPalette';
import { useTabsStore, WELCOME_TAB } from '@/stores/tabs';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { EmptyState } from '@/components/ui/EmptyState';
import { useActiveWorkspace } from '@/features/workspaces/store';
import { ConfirmHost } from '@/components/ui/ConfirmHost';
import { LaunchpadView } from '@/features/launchpad/LaunchpadView';
import { useLaunchpad } from '@/features/launchpad/store';
import { WorkspaceDialogsHost } from '@/features/workspaces/WorkspaceDialogs';
import { Toaster } from '@/components/ui/Toast';
import { StatusBar } from './StatusBar';
import { renderTab } from './tabRegistry';

function MainArea() {
  const { t } = useTranslation();
  const workspace = useActiveWorkspace();
  const activeTab = useTabsStore((s) => s.tabs.find((t) => t.id === s.activeId));
  // The Launchpad covers the tab content while open; the tabs stay as they are underneath.
  const launchpad = useLaunchpad((s) => s.open);
  return (
    <div className="flex h-full min-w-0 flex-col bg-background">
      <TabBar />
      <SessionHealthBanner />
      <main className="min-h-0 flex-1" role="tabpanel">
        {launchpad ? (
          <LaunchpadView />
        ) : activeTab ? (
          <div key={activeTab.id} className="h-full">
            {renderTab(activeTab)}
          </div>
        ) : (
          <EmptyState
            className="h-full"
            title={t('welcome.emptyTitle')}
            description={t('welcome.emptyDescription')}
            action={
              <Button onClick={() => useTabsStore.getState().openTab(WELCOME_TAB)}>
                {workspace ? t('workspaces:switcher.overview') : t('welcome.reopen')}
              </Button>
            }
          />
        )}
      </main>
    </div>
  );
}

export function App() {
  const sidebarHidden = useWorkspaceStore((s) => s.sidebarHidden);
  const sidebarRef = usePanelRef();
  const layout = useSavedLayout('main');
  useEffect(() => {
    if (sidebarHidden) sidebarRef.current?.collapse();
    else sidebarRef.current?.expand();
  }, [sidebarHidden, sidebarRef]);
  return (
    <div className="flex h-full flex-col">
      <div className="min-h-0 flex-1">
        <Group orientation="horizontal" {...layout}>
          <Panel
            id="sidebar"
            panelRef={sidebarRef}
            defaultSize="300px"
            minSize="220px"
            maxSize="45%"
            collapsible
            collapsedSize={0}
            onResize={(size) => {
              useWorkspaceStore.setState({ sidebarHidden: size.inPixels === 0 });
            }}
          >
            <ConnectionsSidebar />
          </Panel>
          <Separator className="w-1 bg-border/50 transition-colors hover:bg-accent data-[separator=active]:bg-accent" />
          <Panel id="main" minSize="40%">
            <MainArea />
          </Panel>
        </Group>
      </div>
      <StatusBar />
      <Toaster />
      <ConfirmHost />
      <WorkspaceDialogsHost />
      {/* App-wide hosts, mounted once: editor shortcuts/dialogs and the destructive confirmation. */}
      <EditorHost />
      <ConfirmDestructiveHost />
      <CommandPalette />
      <AppWindows />
    </div>
  );
}

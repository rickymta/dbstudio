import { useEffect, useLayoutEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/Tabs';
import { cn } from '@/lib/cn';
import { pathKey } from '@/lib/objectPath';
import { useAppSettings } from '@/features/settings/queries';
import { tableTabId, type TableTabPayload } from '@/stores/tabs';
import { layoutStorageKey, loadLayout } from './columnLayout';
import { CellViewer } from './CellViewer';
import { DataGrid } from './DataGrid';
import { DdlViewer } from './DdlViewer';
import { EditBar } from './edit/EditBar';
import { FilterBar } from './FilterBar';
import { GridStatusBar } from './GridStatusBar';
import { ObjectHeader } from './ObjectHeader';
import { StructureTab } from './StructureTab';
import { getGridModel, useTabUi, type SubTab } from './store';

/** Table/view tab: Data (virtualized grid), Structure and DDL sub-tabs. */
export function TableTab({ sessionId, connectionId, path }: TableTabPayload) {
  const { t } = useTranslation('data');
  const tabId = tableTabId(sessionId, path);
  const [ui, patchUi] = useTabUi(tabId);
  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="table-tab">
      <ObjectHeader connectionId={connectionId} path={path} />
      <Tabs
        value={ui.subTab}
        onValueChange={(v) => patchUi({ subTab: v as SubTab })}
        className="flex min-h-0 flex-1 flex-col"
      >
        <TabsList>
          <TabsTrigger value="data">{t('tabs.data')}</TabsTrigger>
          <TabsTrigger value="structure">{t('tabs.structure')}</TabsTrigger>
          <TabsTrigger value="ddl">{t('tabs.ddl')}</TabsTrigger>
        </TabsList>
        {/* Radix mounts a sub-tab only while active, so each loads on first open. */}
        <TabsContent value="data" className="flex flex-col">
          <DataPane tabId={tabId} sessionId={sessionId} connectionId={connectionId} path={path} />
        </TabsContent>
        <TabsContent value="structure">
          <StructureTab sessionId={sessionId} path={path} />
        </TabsContent>
        <TabsContent value="ddl">
          <DdlViewer sessionId={sessionId} connectionId={connectionId} path={path} />
        </TabsContent>
      </Tabs>
    </div>
  );
}

function DataPane({
  tabId,
  sessionId,
  connectionId,
  path,
}: {
  tabId: string;
  sessionId: string;
  connectionId: string;
  path: TableTabPayload['path'];
}) {
  const model = getGridModel(tabId, sessionId, path);
  const [ui, patchUi] = useTabUi(tabId);
  const settings = useAppSettings();
  const layoutKey = layoutStorageKey(connectionId, pathKey(path));

  // Row/cell budget from the settings; takes effect on the next page that gets loaded.
  useEffect(() => {
    model.setLimits(settings.gridMaxRows, settings.gridMaxCells);
  }, [model, settings.gridMaxRows, settings.gridMaxCells]);

  // Read the persisted column layout once per tab (the store keeps it afterwards).
  useLayoutEffect(() => {
    if (!ui.layoutLoaded) patchUi({ layout: loadLayout(layoutKey), layoutLoaded: true });
  }, [ui.layoutLoaded, layoutKey]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    model.ensureLoaded();
  }, [model]);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <FilterBar tabId={tabId} model={model} />
      <EditBar tabId={tabId} sessionId={sessionId} path={path} model={model} />
      <div
        className={cn(
          'flex min-h-0 flex-1',
          ui.viewerPosition === 'right' ? 'flex-row' : 'flex-col',
        )}
      >
        <div className="flex min-h-0 min-w-0 flex-1 flex-col">
          <DataGrid
            tabId={tabId}
            model={model}
            layoutKey={layoutKey}
            timestamp={settings.timestampDisplay}
          />
        </div>
        {ui.viewerOpen && (
          <CellViewer tabId={tabId} model={model} timestamp={settings.timestampDisplay} />
        )}
      </div>
      <GridStatusBar model={model} showActions={false} />
    </div>
  );
}

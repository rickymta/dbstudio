import { useTranslation } from 'react-i18next';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/Tabs';
import type { PlanAdvice, PlanNode, PlanTree } from '@/lib/types';
import type { AdviceStatus } from './adviceModel';
import { AdvicePanel } from './PlanAdvice';
import { PlanDetails } from './PlanDetails';

export type InspectorTab = 'advice' | 'node';

interface Props {
  planId: string;
  tree: PlanTree;
  statement: string;
  selected: PlanNode | undefined;
  ranked: readonly PlanAdvice[];
  status: AdviceStatus;
  tab: InspectorTab;
  onTabChange: (tab: InspectorTab) => void;
  nodeFilter: number | null;
  onClearFilter: () => void;
  expandedId: string | null;
  onActivate: (advice: PlanAdvice) => void;
  onSelectNode: (id: number) => void;
}

/** Content of the inspector: the "Advice" tab and the "Node" tab (the figures of one operator). */
export function PlanAdviceInspector({
  planId,
  tree,
  statement,
  selected,
  ranked,
  status,
  tab,
  onTabChange,
  nodeFilter,
  onClearFilter,
  expandedId,
  onActivate,
  onSelectNode,
}: Props) {
  const { t } = useTranslation('plan');
  return (
    <Tabs
      value={tab}
      onValueChange={(v) => onTabChange(v as InspectorTab)}
      className="flex h-full min-h-0 flex-col"
    >
      <TabsList aria-label={t('inspector.tabs')}>
        <TabsTrigger value="advice">
          {t('advice.tabs.advice', { count: ranked.length })}
        </TabsTrigger>
        <TabsTrigger value="node">{t('advice.tabs.node')}</TabsTrigger>
      </TabsList>
      <TabsContent value="advice" className="min-h-0">
        <AdvicePanel
          planId={planId}
          tree={tree}
          statement={statement}
          ranked={ranked}
          status={status}
          nodeFilter={nodeFilter}
          onClearFilter={onClearFilter}
          expandedId={expandedId}
          onActivate={onActivate}
          onSelectNode={onSelectNode}
        />
      </TabsContent>
      {/* PlanDetails is an <aside> with its own width: the inspector decides it here. */}
      <TabsContent
        value="node"
        className="min-h-0 [&>aside]:h-full [&>aside]:w-full [&>aside]:border-l-0"
      >
        <PlanDetails
          tree={tree}
          selected={selected}
          onSelect={onSelectNode}
          showWarnings={ranked.length === 0}
        />
      </TabsContent>
    </Tabs>
  );
}

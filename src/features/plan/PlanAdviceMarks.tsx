// The lightbulb mark of a node that carries advice (diagram card and table row).

import { Lightbulb } from 'lucide-react';
import { useContext } from 'react';
import { useTranslation } from 'react-i18next';
import { AdviceNodesContext } from './adviceContext';

/** Small lightbulb next to the warning icon of a node that has advice. */
export function AdviceMark({ nodeId, size = 12 }: { nodeId: number; size?: number }) {
  const { t } = useTranslation('plan');
  const nodes = useContext(AdviceNodesContext);
  if (!nodes.has(nodeId)) return null;
  return (
    <Lightbulb
      size={size}
      className="shrink-0 text-accent"
      aria-label={t('advice.nodeHasAdvice')}
      data-testid={`advice-mark-${nodeId}`}
    />
  );
}

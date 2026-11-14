import { Octagon, RefreshCw, PanelRight } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { IconButton } from '@/components/ui/IconButton';
import { Spinner } from '@/components/ui/Spinner';
import { useSessionsStore } from '@/stores/sessions';
import type { GridModel } from './gridModel';
import { useGridVersion } from './useGridVersion';
import { useTabUi } from './store';

export function GridActions({ model, tabId }: { model: GridModel; tabId?: string }) {
  const { t } = useTranslation('data');
  useGridVersion(model);
  const [ui, patchUi] = useTabUi(tabId ?? '');
  const loading = model.status === 'loading';
  const canCancel = useSessionsStore((s) =>
    Object.values(s.sessionsByConnection).some(
      (x) => x.sessionId === model.sessionId && x.descriptor.capabilities.cancelQuery,
    ),
  );
  return (
    <div className="flex shrink-0 items-center gap-1">
      {loading && <Spinner />}
      {loading && canCancel && (
        <Button size="sm" onClick={() => model.stop()}>
          <Octagon size={14} />
          {t('status.stop')}
        </Button>
      )}
      <IconButton
        label={t('status.refresh')}
        icon={<RefreshCw size={15} />}
        disabled={loading}
        onClick={() => model.reload()}
      />
      {tabId && (
        <IconButton
          label={t('viewer.title')}
          icon={<PanelRight size={15} />}
          aria-pressed={ui.viewerOpen}
          onClick={() => patchUi({ viewerOpen: !ui.viewerOpen })}
        />
      )}
    </div>
  );
}

import { useQueryClient } from '@tanstack/react-query';
import { FolderOpen, RefreshCw, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/Badge';
import { Button } from '@/components/ui/Button';
import { Spinner } from '@/components/ui/Spinner';
import { toast } from '@/components/ui/toastStore';
import { describeError } from '@/features/connections/errors';
import { ipc } from '@/lib/ipc';
import type { NativeTool, NativeToolInfo } from '@/lib/types';
import { backupToolsKey, useToolsQuery } from './queries';

/** Native backup tools (pg_dump, mysqldump, ...): detected path/version and manual override. */
export function NativeToolsPanel() {
  const { t } = useTranslation('backup');
  const qc = useQueryClient();
  const tools = useToolsQuery();

  const replace = (info: NativeToolInfo) =>
    qc.setQueryData<NativeToolInfo[]>(backupToolsKey, (old) =>
      (old ?? []).map((x) => (x.tool === info.tool ? info : x)),
    );

  const setPath = async (tool: NativeTool, path: string | null) => {
    try {
      replace(await ipc.backupToolSetPath(tool, path));
    } catch (e) {
      toast.error(describeError(t, e));
    }
  };

  const choose = async (tool: NativeTool) => {
    const path = await ipc.filePickOpen([], false);
    if (path) await setPath(tool, path);
  };

  return (
    <div className="flex flex-col gap-3 p-3" data-testid="native-tools">
      <div className="flex items-center justify-between">
        <p className="text-xs text-muted-foreground">{t('tools.description')}</p>
        <Button size="sm" onClick={() => void tools.refetch()} disabled={tools.isFetching}>
          {tools.isFetching ? <Spinner size={12} /> : <RefreshCw size={12} aria-hidden />}
          {t('tools.detect')}
        </Button>
      </div>
      {tools.isError && (
        <p role="alert" className="text-xs text-danger">
          {describeError(t, tools.error)}
        </p>
      )}
      <ul className="flex flex-col divide-y divide-border rounded-md border border-border">
        {(tools.data ?? []).map((info) => {
          const found = info.path !== null && info.problem === null;
          return (
            <li
              key={info.tool}
              className="flex flex-wrap items-center gap-3 px-3 py-2"
              data-testid={`tool-${info.tool}`}
            >
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-2 text-[13px] font-medium">
                  {t(`tools.name.${info.tool}`)}
                  <Badge tone={found ? 'success' : 'warning'}>
                    {found ? t('tools.found') : t('tools.missing')}
                  </Badge>
                  {found && <Badge>{t(`tools.source.${info.source}`)}</Badge>}
                </div>
                {found ? (
                  <p className="truncate font-mono text-[11px] text-muted-foreground">
                    {info.path}
                    {info.version ? `  -  ${info.version}` : ''}
                  </p>
                ) : (
                  <p className="text-[11px] text-danger">{info.problem ?? t('tools.notFound')}</p>
                )}
              </div>
              <Button size="sm" onClick={() => void choose(info.tool)}>
                <FolderOpen size={12} aria-hidden /> {t('tools.choose')}
              </Button>
              {info.source === 'configured' && info.path !== null && (
                <Button size="sm" variant="ghost" onClick={() => void setPath(info.tool, null)}>
                  <X size={12} aria-hidden /> {t('tools.clear')}
                </Button>
              )}
            </li>
          );
        })}
      </ul>
    </div>
  );
}

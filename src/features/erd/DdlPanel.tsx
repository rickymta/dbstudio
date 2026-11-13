import { useQuery } from '@tanstack/react-query';
import { Check, Copy, X } from 'lucide-react';
import { lazy, Suspense, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { EmptyState } from '@/components/ui/EmptyState';
import { IconButton } from '@/components/ui/IconButton';
import { Spinner } from '@/components/ui/Spinner';
import { useConnectionProfile } from '@/features/connections/queries';
import { ipc } from '@/lib/ipc';
import type { IpcError, ObjectPath, Uuid } from '@/lib/types';
import { erdKeys } from './useErd';

// CodeMirror is a separate chunk, fetched when the first DDL is shown.
const CodeView = lazy(() => import('@/features/data/CodeView'));

interface DdlPanelProps {
  sessionId: Uuid;
  connectionId: Uuid;
  path: ObjectPath;
  title: string;
  onClose: () => void;
}

/** DDL of one table (columns, keys, indexes, foreign keys, triggers) from `ddl_generate`. */
export function DdlPanel({ sessionId, connectionId, path, title, onClose }: DdlPanelProps) {
  const { t } = useTranslation('erd');
  const profile = useConnectionProfile(connectionId);
  const [copied, setCopied] = useState(false);
  const query = useQuery<string, IpcError>({
    queryKey: erdKeys.ddl(sessionId, path),
    queryFn: async () => {
      const items = await ipc.ddlGenerate(sessionId, { scope: path });
      return items.map((i) => i.sql.trimEnd()).join('\n\n');
    },
    retry: false,
  });

  const copy = () => {
    if (query.data === undefined) return;
    void navigator.clipboard
      ?.writeText(query.data)
      .then(() => {
        setCopied(true);
        setTimeout(() => setCopied(false), 1200);
      })
      .catch(() => undefined);
  };

  let body;
  if (query.isPending) {
    body = (
      <div className="flex h-full items-center justify-center gap-2 text-muted-foreground">
        <Spinner /> {t('ddl.loading')}
      </div>
    );
  } else if (query.isError) {
    body = (
      <EmptyState
        className="h-full"
        title={query.error.kind === 'unsupported' ? t('ddl.unsupported') : t('ddl.error')}
        description={query.error.kind === 'unsupported' ? undefined : query.error.message}
      />
    );
  } else {
    body = (
      <Suspense
        fallback={
          <div className="flex h-full items-center justify-center">
            <Spinner />
          </div>
        }
      >
        <CodeView
          value={query.data}
          kind={profile?.kind}
          label={t('ddl.ariaLabel', { name: title })}
        />
      </Suspense>
    );
  }
  return (
    <aside
      aria-label={t('ddl.panel')}
      className="flex h-full w-[420px] shrink-0 flex-col border-l border-border bg-surface"
      data-testid="erd-ddl-panel"
    >
      <header className="flex h-8 shrink-0 items-center gap-1 border-b border-border px-2 text-[13px]">
        <span className="min-w-0 flex-1 truncate font-medium">{title}</span>
        <IconButton
          label={copied ? t('ddl.copied') : t('ddl.copy')}
          icon={copied ? <Check size={13} /> : <Copy size={13} />}
          onClick={copy}
          disabled={query.data === undefined}
        />
        <IconButton label={t('ddl.close')} icon={<X size={13} />} onClick={onClose} />
      </header>
      <div className="min-h-0 flex-1">{body}</div>
    </aside>
  );
}

import { useQuery } from '@tanstack/react-query';
import { Check, Copy, RefreshCw, SquarePen } from 'lucide-react';
import { lazy, Suspense, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { EmptyState } from '@/components/ui/EmptyState';
import { IconButton } from '@/components/ui/IconButton';
import { Spinner } from '@/components/ui/Spinner';
import { connectionColor, useConnectionProfile } from '@/features/connections/queries';
import { openEditor } from '@/features/editor/open';
import { ipc } from '@/lib/ipc';
import { pathKey } from '@/lib/objectPath';
import type { IpcError, ObjectPath, Uuid } from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';

// CodeMirror is a separate chunk, fetched when the first DDL is shown.
const CodeView = lazy(() => import('./CodeView'));

const objectDdlKey = (sessionId: Uuid, path: ObjectPath) =>
  ['objectDdl', sessionId, pathKey(path)] as const;

interface DdlViewerProps {
  sessionId: Uuid;
  connectionId: Uuid;
  path: ObjectPath;
}

/**
 * Read-only, highlighted DDL of an object with Copy, Refresh and "Open in editor" (BROWSE-07).
 * Shared by TableTab and DdlTab.
 */
export function DdlViewer({ sessionId, connectionId, path }: DdlViewerProps) {
  const { t } = useTranslation('data');
  const profile = useConnectionProfile(connectionId);
  const [copied, setCopied] = useState(false);
  const query = useQuery<string, IpcError>({
    queryKey: objectDdlKey(sessionId, path),
    queryFn: () => ipc.objectDdl(sessionId, path),
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

  // The DDL becomes the text of a new query tab in the object's database / schema.
  const openInEditor = () => {
    const session = useSessionsStore.getState().sessionsByConnection[connectionId];
    if (query.data === undefined || session?.sessionId !== sessionId) return;
    const name = path.segments.at(-1)?.name;
    openEditor(session, { path, text: query.data, title: name });
  };

  let body;
  if (query.isPending) {
    body = (
      <div className="flex h-full items-center justify-center gap-2 text-muted-foreground">
        <Spinner /> {t('ddl.loading')}
      </div>
    );
  } else if (query.isError) {
    const unsupported = query.error.kind === 'unsupported';
    body = (
      <EmptyState
        className="h-full"
        title={unsupported ? t('ddl.unsupported') : t('ddl.errorTitle')}
        description={unsupported ? undefined : query.error.message}
        action={
          unsupported ? undefined : (
            <Button size="sm" onClick={() => void query.refetch()}>
              {t('ddl.retry')}
            </Button>
          )
        }
      />
    );
  } else {
    body = (
      <Suspense
        fallback={
          <div className="flex h-full items-center justify-center gap-2 text-muted-foreground">
            <Spinner /> {t('ddl.editorLoading')}
          </div>
        }
      >
        <CodeView value={query.data} kind={profile?.kind} label={t('ddl.ariaLabel')} />
      </Suspense>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col" data-connection-color={connectionColor(profile)}>
      <div className="flex h-8 shrink-0 items-center justify-end gap-1 border-b border-border px-2">
        <Button size="sm" variant="ghost" onClick={copy} disabled={query.data === undefined}>
          {copied ? <Check size={12} /> : <Copy size={12} />}
          {copied ? t('ddl.copied') : t('ddl.copy')}
        </Button>
        <Button
          size="sm"
          variant="ghost"
          onClick={openInEditor}
          disabled={query.data === undefined}
        >
          <SquarePen size={12} />
          {t('ddl.openInEditor')}
        </Button>
        <IconButton
          label={t('ddl.refresh')}
          icon={<RefreshCw size={13} className={query.isFetching ? 'animate-spin' : undefined} />}
          onClick={() => void query.refetch()}
          disabled={query.isFetching}
        />
      </div>
      <div className="min-h-0 flex-1">{body}</div>
    </div>
  );
}

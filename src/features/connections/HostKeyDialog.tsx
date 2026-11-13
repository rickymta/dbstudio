import { ShieldAlert } from 'lucide-react';
import { useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Spinner } from '@/components/ui/Spinner';
import { ipc, toIpcError } from '@/lib/ipc';
import type { HostKeyPrompt } from './hostKeyPrompt';

export interface HostKeyDialogProps {
  /** `null` = closed. */
  prompt: HostKeyPrompt | null;
  onCancel: () => void;
  /** Called after the key was stored (`ssh_trust_host`). */
  onTrusted: () => void;
}

/** Asks whether to trust the host key of an SSH server (unknown or changed). */
export function HostKeyDialog({ prompt, onCancel, onTrusted }: HostKeyDialogProps) {
  return (
    <Dialog open={prompt !== null} onOpenChange={(open) => !open && onCancel()}>
      {prompt && (
        <HostKeyBody
          key={`${prompt.host}:${prompt.port}:${prompt.fingerprint}`}
          prompt={prompt}
          onCancel={onCancel}
          onTrusted={onTrusted}
        />
      )}
    </Dialog>
  );
}

function HostKeyBody({
  prompt,
  onCancel,
  onTrusted,
}: {
  prompt: HostKeyPrompt;
  onCancel: () => void;
  onTrusted: () => void;
}) {
  const { t } = useTranslation('ssh');
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);

  const trust = async () => {
    setBusy(true);
    setFailure(null);
    try {
      await ipc.sshTrustHost(prompt.host, prompt.port, prompt.keyType, prompt.fingerprint);
      onTrusted();
    } catch (e) {
      setFailure(t('hostKey.failed', { message: toIpcError(e).message }));
      setBusy(false);
    }
  };

  const Icon = ShieldAlert;
  return (
    <DialogContent
      title={prompt.changed ? t('hostKey.titleChanged') : t('hostKey.titleNew')}
      description={t(prompt.changed ? 'hostKey.descriptionChanged' : 'hostKey.descriptionNew', {
        host: prompt.host,
        port: prompt.port,
      })}
      className="w-[min(480px,94vw)]"
      footer={
        <>
          <Button onClick={onCancel} disabled={busy}>
            {t('hostKey.cancel')}
          </Button>
          <Button variant="primary" onClick={() => void trust()} disabled={busy}>
            {busy && <Spinner size={12} className="text-accent-foreground" />}
            {prompt.changed ? t('hostKey.trustChanged') : t('hostKey.trust')}
          </Button>
        </>
      }
    >
      <div className="flex flex-col gap-3">
        {prompt.changed && (
          <p
            role="note"
            className="flex items-start gap-2 rounded-md bg-danger-soft px-2 py-1.5 text-xs text-danger"
          >
            <Icon size={16} className="mt-px shrink-0" aria-hidden />
            <span className="min-w-0">{t('hostKey.warningChanged')}</span>
          </p>
        )}
        <dl className="grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1.5 text-xs">
          <Row label={t('hostKey.server')}>
            {prompt.host}:{prompt.port}
          </Row>
          <Row label={t('hostKey.keyType')}>{prompt.keyType}</Row>
          <Row label={t('hostKey.fingerprint')}>
            <code data-testid="host-key-fingerprint" className="break-all select-text">
              {prompt.fingerprint}
            </code>
          </Row>
        </dl>
        {failure && (
          <p role="alert" className="text-xs text-danger">
            {failure}
          </p>
        )}
      </div>
    </DialogContent>
  );
}

function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <>
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="min-w-0">{children}</dd>
    </>
  );
}

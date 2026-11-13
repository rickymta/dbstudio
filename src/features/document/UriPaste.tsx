import { Link2 } from 'lucide-react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Input } from '@/components/ui/Input';
import { Label } from '@/components/ui/Label';
import { Spinner } from '@/components/ui/Spinner';
import type { ConnectionForm } from '@/features/connections/connectionForm';
import { ipc, toIpcError } from '@/lib/ipc';
import { uriFormPatch } from './uriForm';

/** "Paste URI" row of the connection dialog: parses a connection string into the form. */
export function UriPaste({
  id,
  onParsed,
}: {
  id: string;
  onParsed: (patch: Partial<ConnectionForm>) => void;
}) {
  const { t } = useTranslation('document');
  const [uri, setUri] = useState('');
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<{ ok: boolean; text: string } | null>(null);

  const parse = async () => {
    if (uri.trim() === '' || busy) return;
    setBusy(true);
    setMessage(null);
    try {
      const parsed = await ipc.connectionParseUri(uri.trim());
      onParsed(uriFormPatch(parsed));
      setUri('');
      setMessage({
        ok: true,
        text: parsed.hasPassword ? t('uri.appliedPassword') : t('uri.applied'),
      });
    } catch (e) {
      setMessage({ ok: false, text: toIpcError(e).message });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex min-w-0 flex-col gap-1">
      <Label htmlFor={id}>{t('uri.label')}</Label>
      <div className="flex items-center gap-1">
        <Input
          id={id}
          autoComplete="off"
          spellCheck={false}
          value={uri}
          placeholder={t('uri.placeholder')}
          onChange={(e) => setUri(e.target.value)}
          onKeyDown={(e) => {
            // Enter parses here instead of submitting the whole connection form.
            if (e.key === 'Enter') {
              e.preventDefault();
              e.stopPropagation();
              void parse();
            }
          }}
          className="font-mono text-xs"
        />
        <Button onClick={() => void parse()} disabled={busy || uri.trim() === ''}>
          {busy ? <Spinner size={12} /> : <Link2 size={13} />}
          {t('uri.parse')}
        </Button>
      </div>
      {message && (
        <p
          role={message.ok ? 'status' : 'alert'}
          className={
            message.ok ? 'text-xs text-muted-foreground' : 'text-xs break-words text-danger'
          }
        >
          {message.text}
        </p>
      )}
    </div>
  );
}

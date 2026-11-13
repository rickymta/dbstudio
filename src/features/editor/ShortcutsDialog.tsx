import { useTranslation } from 'react-i18next';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { SHORTCUTS, formatKeys } from './shortcuts';
import { useShortcutsHelp } from './shortcutsHelpStore';

/** Cheat-sheet of the keyboard shortcuts (APP-04). */
export function ShortcutsDialog() {
  const { t } = useTranslation('editor');
  const { open, setOpen } = useShortcutsHelp();
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogContent title={t('shortcuts.title')} description={t('shortcuts.description')}>
        <table className="w-full text-[13px]">
          <tbody>
            {SHORTCUTS.map((s) => (
              <tr key={s.id} className="border-b border-border last:border-0">
                <td className="py-1.5 pr-3">{t(`shortcuts.action.${s.id}`)}</td>
                <td className="py-1.5 pr-3 text-xs text-muted-foreground">
                  {t(`shortcuts.scope.${s.scope}`)}
                </td>
                <td className="py-1.5 text-right">
                  <kbd className="rounded border border-border bg-surface-muted px-1.5 py-0.5 font-mono text-xs">
                    {formatKeys(s.keys)}
                  </kbd>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </DialogContent>
    </Dialog>
  );
}

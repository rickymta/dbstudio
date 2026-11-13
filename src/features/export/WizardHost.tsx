import type { ReactNode } from 'react';
import { ExportWizard } from './ExportWizard';
import { ImportWizard } from './ImportWizard';
import { useWizardStore } from './open';

/** Renders the wizard that `openExport` / `openImport` asked for. */
export function WizardHost(): ReactNode {
  const exportOrigin = useWizardStore((s) => s.exportOrigin);
  const importOrigin = useWizardStore((s) => s.importOrigin);
  const setExport = useWizardStore((s) => s.setExport);
  const setImport = useWizardStore((s) => s.setImport);
  return (
    <>
      {exportOrigin && (
        <ExportWizard open onOpenChange={(o) => !o && setExport(null)} origin={exportOrigin} />
      )}
      {importOrigin && (
        <ImportWizard
          open
          onOpenChange={(o) => !o && setImport(null)}
          session={importOrigin.session}
          {...(importOrigin.target ? { target: importOrigin.target } : {})}
          {...(importOrigin.parent ? { parent: importOrigin.parent } : {})}
        />
      )}
    </>
  );
}

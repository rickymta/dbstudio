import { ListChecks, Loader2 } from 'lucide-react';
import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { WizardHost } from '@/features/export/WizardHost';
import { JobsPanel, ProgressBar } from './JobsPanel';
import { isActive, jobFraction, useJobsStore, useJobsSync } from './jobsStore';

/**
 * Status bar indicator of the background jobs: running count and overall progress, opens the
 * job center. It also hosts the export / import wizards, which are opened from anywhere
 * (`openExport`, `openImport`) and need a place in the tree that is always mounted.
 */
export function JobsIndicator(): ReactNode {
  const { t } = useTranslation('jobs');
  useJobsSync();
  const jobs = useJobsStore((s) => s.jobs);
  const setOpen = useJobsStore((s) => s.setPanelOpen);
  const all = Object.values(jobs);
  const running = all.filter(isActive);
  const fractions = running.map(jobFraction);
  const known = fractions.filter((f): f is number => f !== null);
  const overall =
    running.length > 0 && known.length === running.length
      ? known.reduce((a, b) => a + b, 0) / known.length
      : null;
  const failed = all.filter((j) => j.status === 'failed').length;
  const label =
    running.length > 0 ? t('indicator.running', { count: running.length }) : t('indicator.idle');
  return (
    <>
      <Button
        variant="ghost"
        size="sm"
        data-testid="jobs-indicator"
        aria-label={label}
        onClick={() => setOpen(true)}
      >
        {running.length > 0 ? (
          <Loader2 size={13} className="animate-spin text-accent" />
        ) : (
          <ListChecks size={13} />
        )}
        {running.length > 0 && (
          <>
            <span>{label}</span>
            <span className="w-16">
              <ProgressBar fraction={overall} label={t('progress')} />
            </span>
          </>
        )}
        {running.length === 0 && failed > 0 && (
          <span className="text-danger">{t('indicator.failed', { count: failed })}</span>
        )}
      </Button>
      <JobsPanel />
      <WizardHost />
    </>
  );
}

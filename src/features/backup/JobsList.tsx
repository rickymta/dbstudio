import { Pencil, Play, Trash2 } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/Badge';
import { IconButton } from '@/components/ui/IconButton';
import { Switch } from '@/components/ui/Switch';
import type { BackupJob, ConnectionProfile } from '@/lib/types';
import { formatDateTime } from './format';
import { scheduleText } from './schedule';

export interface JobsListProps {
  jobs: BackupJob[];
  profiles: ConnectionProfile[];
  /** Connections are still loading: do not call them deleted yet. */
  loadingProfiles?: boolean;
  onRun: (job: BackupJob) => void;
  onEdit: (job: BackupJob) => void;
  onDelete: (job: BackupJob) => void;
  onToggle: (job: BackupJob, enabled: boolean) => void;
}

export function JobsList({
  jobs,
  profiles,
  loadingProfiles,
  onRun,
  onEdit,
  onDelete,
  onToggle,
}: JobsListProps) {
  const { t, i18n } = useTranslation('backup');
  return (
    <table className="w-full border-collapse text-xs" data-testid="jobs-list">
      <thead>
        <tr className="border-b border-border text-left text-muted-foreground">
          <th className="py-1 pr-2">{t('jobs.col.name')}</th>
          <th className="pr-2">{t('jobs.col.connection')}</th>
          <th className="pr-2">{t('jobs.col.schedule')}</th>
          <th className="pr-2">{t('jobs.col.lastRun')}</th>
          <th className="pr-2">{t('jobs.col.nextRun')}</th>
          <th className="pr-2">{t('jobs.col.enabled')}</th>
          <th />
        </tr>
      </thead>
      <tbody>
        {jobs.map((job) => {
          const profile = profiles.find((p) => p.id === job.spec.connectionId);
          return (
            <tr key={job.id} className="border-b border-border/50 align-top" data-testid="job-row">
              <td className="py-1.5 pr-2 font-medium">
                {job.name}
                {job.spec.encrypted && (
                  <Badge className="ml-1.5" tone="success">
                    {t('jobs.encrypted')}
                  </Badge>
                )}
              </td>
              <td className="pr-2">
                {profile?.name ?? (loadingProfiles ? '...' : t('jobs.unknownConnection'))}
              </td>
              <td className="pr-2">
                <span>{scheduleText(job.schedule, i18n.language, t('jobs.manual'))}</span>
                {job.schedule && (
                  <span className="ml-1 text-muted-foreground">
                    ({t(`cron.tz.${job.schedule.timezone}`)})
                  </span>
                )}
              </td>
              <td className="pr-2 whitespace-nowrap">
                {formatDateTime(job.lastRunAt, i18n.language)}
              </td>
              <td className="pr-2 whitespace-nowrap">
                {job.enabled && job.schedule ? formatDateTime(job.nextRunAt, i18n.language) : '-'}
              </td>
              <td className="pr-2">
                <Switch
                  checked={job.enabled}
                  onCheckedChange={(v) => onToggle(job, v)}
                  aria-label={t('jobs.toggle', { name: job.name })}
                />
              </td>
              <td className="whitespace-nowrap text-right">
                <IconButton
                  label={t('jobs.runNow')}
                  icon={<Play size={13} />}
                  onClick={() => onRun(job)}
                />
                <IconButton
                  label={t('jobs.edit')}
                  icon={<Pencil size={13} />}
                  onClick={() => onEdit(job)}
                />
                <IconButton
                  label={t('jobs.delete')}
                  icon={<Trash2 size={13} />}
                  onClick={() => onDelete(job)}
                />
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}

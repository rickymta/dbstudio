import { beforeEach, describe, expect, it } from 'vitest';
import { ipc } from '@/lib/ipc';
import { resetMockConnections, resetMockEvents, resetMockPhase2, mockListen } from '@/lib/mock';
import { MOCK_PASSPHRASE, nextCronRuns } from '@/lib/mock/backup';
import type { IpcError } from '@/lib/types';

beforeEach(() => {
  resetMockPhase2();
  resetMockConnections();
  resetMockEvents();
});

const FROM = new Date(Date.UTC(2026, 10, 14, 10, 7, 30));

describe('nextCronRuns', () => {
  it('computes daily and stepped schedules in UTC', () => {
    expect(nextCronRuns('30 2 * * *', 'utc', 2, FROM)).toEqual([
      '2026-11-15T02:30:00.000Z',
      '2026-11-16T02:30:00.000Z',
    ]);
    expect(nextCronRuns('*/20 10-11 * * *', 'utc', 4, FROM)).toEqual([
      '2026-11-14T10:20:00.000Z',
      '2026-11-14T10:40:00.000Z',
      '2026-11-14T11:00:00.000Z',
      '2026-11-14T11:20:00.000Z',
    ]);
  });

  it('supports weekday lists and day-of-month', () => {
    // 2026-11-14 is a Saturday: next Monday and Wednesday.
    expect(nextCronRuns('0 6 * * 1,3', 'utc', 2, FROM)).toEqual([
      '2026-11-16T06:00:00.000Z',
      '2026-11-18T06:00:00.000Z',
    ]);
    expect(nextCronRuns('0 0 1 * *', 'utc', 2, FROM)).toEqual([
      '2026-12-01T00:00:00.000Z',
      '2027-01-01T00:00:00.000Z',
    ]);
  });

  it('rejects invalid expressions and time zones', () => {
    for (const c of [
      '',
      '* * * *',
      '60 * * * *',
      '* 24 * * *',
      'a * * * *',
      '*/0 * * * *',
      '5-1 * * * *',
    ]) {
      expect(() => nextCronRuns(c, 'utc', 1, FROM)).toThrow();
    }
    expect(() => nextCronRuns('* * * * *', 'mars', 1, FROM)).toThrow();
  });

  it('is exposed through ipc.schedulePreview with kind "other" on error', async () => {
    const runs = await ipc.schedulePreview('0 3 * * *', 'utc', 5);
    expect(runs).toHaveLength(5);
    await expect(ipc.schedulePreview('bad', 'utc', 5)).rejects.toMatchObject({ kind: 'other' });
  });
});

describe('backup mock', () => {
  const connectionId = '00000000-0000-4000-8000-0000000000b1';
  const prodId = '00000000-0000-4000-8000-0000000000b3';

  it('inspects encrypted files with a passphrase and throws integrity when it is wrong', async () => {
    const locked = await ipc.backupInspect('/b/x-enc.dbsbak', null);
    expect(locked).toMatchObject({ encrypted: true, preamble: null });
    await expect(ipc.backupInspect('/b/x-enc.dbsbak', 'nope')).rejects.toMatchObject({
      kind: 'integrity',
    });
    const ok = await ipc.backupInspect('/b/x-enc.dbsbak', MOCK_PASSPHRASE);
    expect(ok.header).not.toBeNull();
    expect((await ipc.backupInspect('/b/plain.dbsbak', null)).encrypted).toBe(false);
  });

  it('requires confirmation to restore into production', async () => {
    const spec = { file: '/b/plain.dbsbak', targetConnectionId: prodId, targetDatabase: 'app' };
    // read-only profile is refused outright
    await expect(ipc.restoreStart(spec, null)).rejects.toMatchObject({ kind: 'readOnly' });
    const dev = { ...spec, targetConnectionId: connectionId, clean: true };
    const err = (await ipc.restoreStart(dev, null).catch((e: IpcError) => e)) as IpcError;
    expect(err.kind).toBe('confirmationRequired');
    const c = err.confirmation!;
    await expect(
      ipc.restoreStart(dev, null, { token: c.token, typed: 'wrong' }),
    ).rejects.toMatchObject({ kind: 'confirmationRequired' });
  });

  it('runs a job now: emits progress, finishes and records a run', async () => {
    const events: string[] = [];
    mockListen('job://progress', (e) => events.push((e as { type: string }).type));
    const job = await ipc.backupJobSave(
      {
        id: 'job-1',
        name: 'Nightly',
        enabled: true,
        spec: {
          connectionId,
          targets: [],
          engine: { kind: 'logical' },
          destination: { kind: 'localDir', path: '/b' },
        },
        schedule: { cron: '0 2 * * *', timezone: 'utc' },
        lastRunAt: null,
        nextRunAt: null,
        createdAt: 'x',
        updatedAt: 'x',
      },
      { action: 'clear' },
    );
    expect(job.nextRunAt).not.toBeNull();
    await new Promise<void>((resolve) => {
      mockListen('job://progress', (e) => (e as { type: string }).type === 'finished' && resolve());
      void ipc.backupJobRunNow('job-1');
    });
    expect(events[0]).toBe('started');
    expect(events).toContain('progress');
    const runs = await ipc.backupRunsList('job-1', 10);
    expect(runs).toHaveLength(1);
    expect(runs[0]).toMatchObject({ status: 'succeeded', kind: 'backup', jobName: 'Nightly' });
    expect((await ipc.backupJobsList())[0]!.lastRunAt).not.toBeNull();
  });

  it('detects tools with a problem for missing ones and stores a configured path', async () => {
    const tools = await ipc.backupToolsDetect();
    expect(tools.find((t) => t.tool === 'pgDump')?.problem).toBeNull();
    expect(tools.find((t) => t.tool === 'mySqlDump')?.problem).toBe('not found');
    const set = await ipc.backupToolSetPath('mySqlDump', '/opt/mysqldump');
    expect(set).toMatchObject({ path: '/opt/mysqldump', source: 'configured', problem: null });
    expect((await ipc.backupToolSetPath('mySqlDump', null)).problem).toBe('not found');
  });
});

import { describe, expect, it } from 'vitest';
import {
  buildJob,
  buildSpec,
  emptyForm,
  fileNameExample,
  firstErrorSection,
  formFromJob,
  formatDate,
  passphraseChange,
  unknownPlaceholders,
  validateForm,
  type JobForm,
} from './jobForm';

const NOW = new Date(2026, 10, 14, 9, 5, 7);

function validForm(over: Partial<JobForm> = {}): JobForm {
  return {
    ...emptyForm(),
    name: 'Nightly',
    connectionId: 'conn-1',
    database: 'shop',
    directory: '/backups',
    ...over,
  };
}

describe('fileNameExample', () => {
  it('expands placeholders and the date pattern', () => {
    expect(
      fileNameExample(
        '{connection}-{database}-{date:%Y%m%d-%H%M%S}-{job}',
        { connection: 'My DB', database: 'shop', job: 'Nightly' },
        NOW,
      ),
    ).toBe('My_DB-shop-20261114-090507-Nightly.dbsbak');
  });

  it('uses the default date pattern and keeps unknown placeholders', () => {
    expect(fileNameExample('x-{date}-{nope}', { connection: '', database: '', job: '' }, NOW)).toBe(
      'x-20261114-090507-{nope}.dbsbak',
    );
    expect(formatDate('%d/%m/%Y', NOW)).toBe('14/11/2026');
    expect(unknownPlaceholders('{job}{nope}{date:%Y}')).toEqual(['nope']);
  });
});

describe('validateForm', () => {
  it('accepts a complete form', () => {
    expect(validateForm(validForm())).toEqual({});
  });

  it('reports missing name, connection, database and directory', () => {
    const errors = validateForm(emptyForm());
    expect(errors).toMatchObject({
      name: 'nameRequired',
      connection: 'connectionRequired',
      database: 'databaseRequired',
      directory: 'directoryRequired',
    });
    expect(firstErrorSection(errors)).toBe('source');
  });

  it('does not require a name or schedule for a one-off backup', () => {
    expect(validateForm(validForm({ name: '', cron: 'bad' }), { once: true })).toEqual({});
  });

  it('checks passphrase presence and confirmation', () => {
    expect(validateForm(validForm({ encrypted: true })).passphrase).toBe('passphraseRequired');
    expect(
      validateForm(validForm({ encrypted: true, passphrase: 'a', passphrase2: 'b' })).passphrase,
    ).toBe('passphraseMismatch');
    expect(validateForm(validForm({ encrypted: true, passphrase: 'a', passphrase2: 'a' }))).toEqual(
      {},
    );
    // Editing an encrypted job: empty fields keep the stored passphrase.
    expect(validateForm(validForm({ encrypted: true }), { hadPassphrase: true })).toEqual({});
  });

  it('checks the cron expression, retention and template', () => {
    expect(validateForm(validForm({ cron: '0 2 * *' })).cron).toBe('cronInvalid');
    expect(validateForm(validForm({ scheduled: false, cron: 'x' })).cron).toBeUndefined();
    expect(validateForm(validForm({ keepLast: '0' })).keepLast).toBe('retentionInvalid');
    expect(validateForm(validForm({ maxAgeDays: 'abc' })).maxAgeDays).toBe('retentionInvalid');
    expect(validateForm(validForm({ keepLast: '7', maxAgeDays: '30' }))).toEqual({});
    expect(validateForm(validForm({ fileTemplate: '../{job}' })).fileTemplate).toBe(
      'templateSeparator',
    );
    expect(validateForm(validForm({ fileTemplate: '{oops}' })).fileTemplate).toBe(
      'templateUnknown',
    );
  });

  it('requires a usable native tool', () => {
    const f = validForm({ engineKind: 'native' });
    expect(validateForm(f).nativeTool).toBe('toolRequired');
    expect(
      validateForm({ ...f, nativeTool: 'mySqlDump' }, { toolUsable: () => false }).nativeTool,
    ).toBe('toolMissing');
    expect(validateForm({ ...f, nativeTool: 'pgDump' }, { toolUsable: () => true })).toEqual({});
  });
});

describe('building the payload', () => {
  it('builds the spec, schedule and secret change', () => {
    const form = validForm({
      encrypted: true,
      passphrase: 'pw',
      passphrase2: 'pw',
      keepLast: '5',
      timezone: 'utc',
    });
    const job = buildJob(form, null, 'new-id', '2026-11-14T00:00:00Z');
    expect(job).toMatchObject({
      id: 'new-id',
      name: 'Nightly',
      enabled: true,
      schedule: { cron: '0 2 * * *', timezone: 'utc' },
      retention: { keepLast: 5, maxAgeDays: null },
      missedRun: 'runOnce',
      notify: { onSuccess: false, onFailure: true },
    });
    expect(job.spec).toEqual({
      connectionId: 'conn-1',
      targets: [{ segments: [{ kind: 'database', name: 'shop' }] }],
      engine: { kind: 'logical', content: 'both' },
      compression: { kind: 'gzip', level: 6 },
      encrypted: true,
      destination: {
        kind: 'localDir',
        path: '/backups',
        fileTemplate: '{connection}-{database}-{date:%Y%m%d-%H%M%S}',
      },
    });
    expect(passphraseChange(form)).toEqual({ action: 'set', value: 'pw' });
    expect(passphraseChange(validForm({ encrypted: true }))).toEqual({ action: 'keep' });
    expect(passphraseChange(validForm())).toEqual({ action: 'clear' });
  });

  it('writes native server-side tools to a server directory', () => {
    const spec = buildSpec(validForm({ engineKind: 'native', nativeTool: 'msSqlServerBackup' }));
    expect(spec.engine).toEqual({ kind: 'native', tool: 'msSqlServerBackup' });
    expect(spec.destination).toEqual({ kind: 'serverDir', path: '/backups' });
  });

  it('round-trips a job through the form', () => {
    const job = buildJob(
      validForm({ scheduled: false, compression: 'zstd', level: 10, maxAgeDays: '14' }),
      null,
      'id',
      '2026-11-14T00:00:00Z',
    );
    const back = formFromJob({ ...job, lastRunAt: 'a', nextRunAt: 'b' });
    expect(back).toMatchObject({
      name: 'Nightly',
      database: 'shop',
      scheduled: false,
      compression: 'zstd',
      level: 10,
      maxAgeDays: '14',
      keepLast: '',
    });
    const rebuilt = buildJob(back, job, 'other', '2027-01-01T00:00:00Z');
    expect(rebuilt.id).toBe('id');
    expect(rebuilt.createdAt).toBe('2026-11-14T00:00:00Z');
    expect(rebuilt.spec).toEqual(job.spec);
  });
});

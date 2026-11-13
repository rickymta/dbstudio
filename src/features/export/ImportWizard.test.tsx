import { cleanup, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { PG_CONNECTION, pgPath, renderWithProviders } from '@/features/data/testUtils';
import { useJobsStore } from '@/features/jobs/jobsStore';
import { ipc } from '@/lib/ipc';
import { resetMockEvents } from '@/lib/mock';
import { resetMockExport } from '@/lib/mock/export';
import { mockJobTiming, resetMockJobs } from '@/lib/mock/jobs';
import type { ImportSpec, SessionInfo } from '@/lib/types';
import { ImportWizard } from './ImportWizard';

const PROD_CONNECTION = '00000000-0000-4000-8000-0000000000b3';
let session: SessionInfo;

beforeEach(async () => {
  session = await ipc.sessionOpen(PG_CONNECTION);
  useJobsStore.setState({ jobs: {}, panelOpen: false });
  mockJobTiming.stepMs = 5;
});

afterEach(() => {
  cleanup();
  resetMockExport();
  resetMockJobs();
  resetMockEvents();
  vi.restoreAllMocks();
});

const click = (name: RegExp | string) =>
  userEvent.setup().click(screen.getByRole('button', { name }));

async function pickFile(path: string) {
  vi.spyOn(ipc, 'filePickOpen').mockResolvedValue(path);
  await click(/Browse/);
  await waitFor(() => expect(screen.getByLabelText('File')).toHaveValue(path));
}

describe('ImportWizard', () => {
  it('previews a CSV, maps columns onto the table and starts the job', async () => {
    const user = userEvent.setup();
    const start = vi.spyOn(ipc, 'importStart');
    const target = pgPath('users');
    renderWithProviders(
      <ImportWizard open onOpenChange={() => {}} session={session} target={target} />,
    );
    await pickFile('/tmp/in.csv');
    // The preview detected `;` and shows the rows.
    expect(await screen.findByTestId('import-preview')).toHaveTextContent('Widget');
    await waitFor(() => expect(screen.getByLabelText('Delimiter')).toHaveValue(';'));
    await click(/Next/);
    await click(/Next/);
    // `id` maps by name; `name` is mapped by hand to `full_name`.
    const mapping = await screen.findByTestId('import-mapping');
    await waitFor(() => expect(within(mapping).getAllByRole('row')).toHaveLength(5));
    await user.click(screen.getByRole('combobox', { name: 'Transform of name' }));
    await user.click(await screen.findByRole('option', { name: 'Trim, empty as NULL' }));
    await user.click(screen.getByRole('combobox', { name: 'Column for name' }));
    await user.click(await screen.findByRole('option', { name: 'full_name' }));
    await user.click(screen.getByRole('button', { name: 'Start import' }));

    await waitFor(() => expect(start).toHaveBeenCalledTimes(1));
    const spec = start.mock.calls[0]![0] as ImportSpec;
    expect(spec.file).toBe('/tmp/in.csv');
    expect(spec.format).toMatchObject({ kind: 'csv', delimiter: ';', header: true });
    expect(spec.target).toEqual({ kind: 'existing', table: target });
    expect(spec.mapping).toEqual([
      { source: 'id', target: 'id', transform: 'none' },
      { source: 'name', target: 'full_name', transform: 'trimEmptyAsNull' },
    ]);
    expect(spec.mode).toEqual({ kind: 'batches', rows: 5000 });
    expect(spec.maxErrors).toBe(0);
    expect(useJobsStore.getState().panelOpen).toBe(true);
  });

  it('creates a new table from the file with proposed types', async () => {
    const user = userEvent.setup();
    const start = vi.spyOn(ipc, 'importStart');
    const parent = { segments: pgPath('users').segments.slice(0, 3) };
    renderWithProviders(
      <ImportWizard open onOpenChange={() => {}} session={session} parent={parent} />,
    );
    await pickFile('/tmp/in.csv');
    await screen.findByTestId('import-preview');
    await click(/Next/);
    await user.type(screen.getByLabelText('Name of the new table'), 'prices');
    await click(/Next/);
    const columns = await screen.findByTestId('import-new-columns');
    expect(within(columns).getByLabelText('Column name 3')).toHaveValue('price');
    expect(within(columns).getByLabelText('Type 3')).toHaveValue('double precision');
    expect(within(columns).getByLabelText('Type 1')).toHaveValue('bigint');
    await user.click(within(columns).getByLabelText('Key 1'));
    await user.click(screen.getByRole('button', { name: 'Start import' }));
    await waitFor(() => expect(start).toHaveBeenCalled());
    const spec = start.mock.calls[0]![0] as ImportSpec;
    expect(spec.target).toMatchObject({ kind: 'new', parent, name: 'prices' });
    const t = spec.target as Extract<ImportSpec['target'], { kind: 'new' }>;
    expect(t.columns[0]).toEqual({
      name: 'id',
      typeName: 'bigint',
      nullable: false,
      primaryKey: true,
    });
    expect(spec.mapping?.[2]).toEqual({ source: 'price', target: 'price' });
  });

  it('lists the statements of a SQL script and starts it right away', async () => {
    const start = vi.spyOn(ipc, 'importStart');
    renderWithProviders(<ImportWizard open onOpenChange={() => {}} session={session} />);
    await pickFile('/tmp/dump.sql');
    const list = await screen.findByTestId('import-statements');
    expect(list).toHaveTextContent('CREATE TABLE');
    expect(list).toHaveTextContent('DROP TABLE');
    await click('Start import');
    await waitFor(() => expect(start).toHaveBeenCalled());
    const spec = start.mock.calls[0]![0] as ImportSpec;
    expect(spec.format).toEqual({ kind: 'sql', continueOnError: false });
    expect(spec.target).toEqual({ kind: 'script', database: null });
  });

  it('asks the user to type the confirmation text on a production connection', async () => {
    const user = userEvent.setup();
    const prod = await ipc.sessionOpen(PROD_CONNECTION, 'secret');
    const start = vi.spyOn(ipc, 'importStart');
    renderWithProviders(<ImportWizard open onOpenChange={() => {}} session={prod} />);
    await pickFile('/tmp/dump.sql');
    await screen.findByTestId('import-statements');
    await click('Start import');
    const confirm = await screen.findByTestId('import-confirm');
    expect(confirm).toHaveTextContent('Type prod_db to confirm');
    const go = screen.getByRole('button', { name: 'Confirm and import' });
    expect(go).toBeDisabled();
    await user.type(screen.getByLabelText(/Type prod_db/), 'prod_db');
    await user.click(go);
    await waitFor(() => expect(start).toHaveBeenCalledTimes(2));
    expect(start.mock.calls[0]![1]).toBeNull();
    expect(start.mock.calls[1]![1]).toEqual({ token: 'mock-token', typed: 'prod_db' });
  });

  it('shows a failing preview', async () => {
    vi.spyOn(ipc, 'importPreview').mockRejectedValue({
      kind: 'other',
      message: 'cannot read the file',
    });
    renderWithProviders(<ImportWizard open onOpenChange={() => {}} session={session} />);
    await pickFile('/tmp/bad.csv');
    expect(await screen.findByRole('alert')).toHaveTextContent('cannot read the file');
    expect(screen.getByRole('button', { name: /Next/ })).toBeDisabled();
  });
});

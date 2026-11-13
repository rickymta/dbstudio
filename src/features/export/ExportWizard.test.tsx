import { cleanup, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { PG_CONNECTION, pgPath, renderWithProviders } from '@/features/data/testUtils';
import { JobsIndicator } from '@/features/jobs/JobsIndicator';
import { useJobsStore } from '@/features/jobs/jobsStore';
import { ipc } from '@/lib/ipc';
import { resetMockEvents } from '@/lib/mock';
import { resetMockExport } from '@/lib/mock/export';
import { mockJobTiming, resetMockJobs } from '@/lib/mock/jobs';
import type { ExportSpec, SessionInfo } from '@/lib/types';
import { ExportWizard } from './ExportWizard';
import { openExport, useWizardStore } from './open';

let session: SessionInfo;

beforeEach(async () => {
  session = await ipc.sessionOpen(PG_CONNECTION);
  useJobsStore.setState({ jobs: {}, panelOpen: false });
  useWizardStore.setState({ exportOrigin: null, importOrigin: null });
  mockJobTiming.stepMs = 5;
});

afterEach(() => {
  cleanup();
  resetMockExport();
  resetMockJobs();
  resetMockEvents();
  vi.restoreAllMocks();
});

const next = (user: ReturnType<typeof userEvent.setup>) =>
  user.click(screen.getByRole('button', { name: /Next/ }));

describe('ExportWizard', () => {
  it('walks source, format and destination and starts the export job', async () => {
    const user = userEvent.setup();
    const start = vi.spyOn(ipc, 'exportStart');
    const pick = vi.spyOn(ipc, 'filePickSave').mockResolvedValue('/tmp/actor.zip');
    renderWithProviders(
      <ExportWizard open onOpenChange={() => {}} origin={{ session, path: pgPath('actor') }} />,
    );
    // Step 1: the table and the content.
    expect(screen.getByText('actor')).toBeInTheDocument();
    await next(user);
    // Step 2: CSV with a semicolon.
    await user.click(screen.getByRole('combobox', { name: 'Format' }));
    await user.click(await screen.findByRole('option', { name: 'CSV' }));
    const delimiter = screen.getByLabelText('Delimiter');
    await user.clear(delimiter);
    await user.type(delimiter, ';');
    await next(user);
    // Step 3: pick the file; "both" content of CSV gives a zip.
    expect(screen.getByRole('button', { name: 'Start export' })).toBeDisabled();
    await user.click(screen.getByRole('button', { name: /Browse/ }));
    expect(pick).toHaveBeenCalledWith('actor.zip', [{ name: 'ZIP archive', extensions: ['zip'] }]);
    expect(screen.getByLabelText('File')).toHaveValue('/tmp/actor.zip');
    await user.click(screen.getByRole('button', { name: 'Start export' }));

    await waitFor(() => expect(start).toHaveBeenCalledTimes(1));
    const spec = start.mock.calls[0]![0] as ExportSpec;
    expect(spec.connectionId).toBe(PG_CONNECTION);
    expect(spec.source).toEqual({ kind: 'tables', paths: [pgPath('actor')] });
    expect(spec.content).toBe('both');
    expect(spec.format).toMatchObject({ kind: 'csv', delimiter: ';', encoding: 'utf8Bom' });
    expect(spec.destination).toEqual({ path: '/tmp/actor.zip', overwrite: true });
    expect(useJobsStore.getState().panelOpen).toBe(true);
  });

  it('chooses tables of a schema', async () => {
    const user = userEvent.setup();
    const start = vi.spyOn(ipc, 'exportStart');
    vi.spyOn(ipc, 'filePickSave').mockResolvedValue('/tmp/x.xlsx');
    const schema = { segments: pgPath('actor').segments.slice(0, 3) };
    renderWithProviders(
      <ExportWizard open onOpenChange={() => {}} origin={{ session, path: schema }} />,
    );
    await user.click(screen.getByLabelText('Choose tables'));
    expect(screen.getByRole('button', { name: /Next/ })).toBeDisabled();
    const list = await screen.findByTestId('export-tables');
    await user.click(await within(list).findByLabelText('public.actor'));
    await next(user);
    await user.click(screen.getByRole('combobox', { name: 'Format' }));
    await user.click(await screen.findByRole('option', { name: 'Excel (XLSX)' }));
    await user.click(screen.getByLabelText('Data dictionary sheet'));
    await next(user);
    await user.click(screen.getByRole('button', { name: /Browse/ }));
    await user.click(screen.getByRole('button', { name: 'Start export' }));
    await waitFor(() => expect(start).toHaveBeenCalled());
    const spec = start.mock.calls[0]![0] as ExportSpec;
    expect(spec.source.kind).toBe('tables');
    expect(spec.format).toMatchObject({ kind: 'xlsx', dataDictionary: true });
  });

  it('exports a query: one statement, data only, no SQL for document databases', async () => {
    const user = userEvent.setup();
    renderWithProviders(
      <ExportWizard open onOpenChange={() => {}} origin={{ session, query: 'SELECT 1' }} />,
    );
    expect(screen.getByLabelText('Query')).toHaveValue('SELECT 1');
    expect(screen.queryByLabelText('Content')).toBeNull();
    await user.clear(screen.getByLabelText('Query'));
    expect(screen.getByRole('button', { name: /Next/ })).toBeDisabled();
  });

  it('saves a preset and applies it again', async () => {
    const user = userEvent.setup();
    vi.spyOn(ipc, 'filePickSave').mockResolvedValue('/tmp/p.json');
    const origin = { session, path: pgPath('actor') };
    const { unmount } = renderWithProviders(
      <ExportWizard open onOpenChange={() => {}} origin={origin} />,
    );
    await next(user);
    await user.click(screen.getByRole('combobox', { name: 'Format' }));
    await user.click(await screen.findByRole('option', { name: 'JSON' }));
    await user.click(screen.getByLabelText('Indented'));
    await next(user);
    await user.type(screen.getByLabelText('Preset name'), 'pretty json');
    await user.click(screen.getByRole('button', { name: 'Save preset' }));
    const saved = await ipc.exportPresetsList(PG_CONNECTION);
    await waitFor(async () =>
      expect((await ipc.exportPresetsList(PG_CONNECTION)).map((p) => p.name)).toEqual([
        'pretty json',
      ]),
    );
    expect(saved).toBeDefined();
    unmount();

    renderWithProviders(<ExportWizard open onOpenChange={() => {}} origin={origin} />);
    await next(user);
    await next(user);
    await user.click(await screen.findByRole('combobox', { name: 'Apply a preset' }));
    await user.click(await screen.findByRole('option', { name: 'pretty json' }));
    await user.click(screen.getByRole('button', { name: /Back/ }));
    expect(screen.getByRole('combobox', { name: 'Format' })).toHaveTextContent('JSON');
    expect(screen.getByLabelText('Indented')).toBeChecked();
  });

  it('shows the error of a refused export', async () => {
    const user = userEvent.setup();
    vi.spyOn(ipc, 'filePickSave').mockResolvedValue('/tmp/x.sql');
    vi.spyOn(ipc, 'exportStart').mockRejectedValue({
      kind: 'permission',
      message: 'path not granted',
    });
    renderWithProviders(
      <ExportWizard open onOpenChange={() => {}} origin={{ session, path: pgPath('actor') }} />,
    );
    await next(user);
    await next(user);
    await user.click(screen.getByRole('button', { name: /Browse/ }));
    await user.click(screen.getByRole('button', { name: 'Start export' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('path not granted');
  });

  it('runs end to end through openExport and the job center', async () => {
    const user = userEvent.setup();
    vi.spyOn(ipc, 'filePickSave').mockResolvedValue('/tmp/e2e.sql');
    renderWithProviders(<JobsIndicator />);
    await screen.findByTestId('jobs-indicator');
    openExport({ session, path: pgPath('actor') });
    await screen.findByRole('button', { name: /Next/ });
    await next(user);
    await next(user);
    await user.click(await screen.findByRole('button', { name: /Browse/ }));
    await user.click(screen.getByRole('button', { name: 'Start export' }));
    // The panel opens and the job runs to its result.
    const row = await screen.findByTestId(/^job-[0-9a-f-]{36}$/, {}, { timeout: 3000 });
    await waitFor(() => expect(within(row).getByTestId('job-status')).toHaveTextContent('Done'), {
      timeout: 3000,
    });
    expect(row).toHaveTextContent('2550 row(s)');
    expect(row).toHaveTextContent('/tmp/e2e.sql');
    expect(useWizardStore.getState().exportOrigin).toBeNull();
  });
});

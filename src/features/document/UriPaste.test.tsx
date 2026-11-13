import { cleanup, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { renderWithProviders } from '@/features/data/testUtils';
import { ipc } from '@/lib/ipc';
import { mockParseMongoUri } from '@/lib/mock/catalogMongo';
import { UriPaste } from './UriPaste';

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

// The shared browser mock leaves `connectionParseUri` unsupported; use the Mongo one here.
const useMongoParser = () =>
  vi.spyOn(ipc, 'connectionParseUri').mockImplementation(mockParseMongoUri);

describe('UriPaste', () => {
  it('fills form fields from a parsed URI', async () => {
    useMongoParser();
    const onParsed = vi.fn();
    const user = userEvent.setup();
    renderWithProviders(<UriPaste id="u" onParsed={onParsed} />);
    await user.type(
      screen.getByLabelText('Connection URI'),
      'mongodb://ada:pw@db.local:27018/app{Enter}',
    );
    await waitFor(() => expect(onParsed).toHaveBeenCalled());
    expect(onParsed.mock.calls[0]![0]).toMatchObject({
      kind: 'mongodb',
      host: 'db.local',
      port: '27018',
      database: 'app',
      username: 'ada',
    });
    expect(await screen.findByText(/Type the password again/)).toBeInTheDocument();
  });

  it('shows the error of an invalid URI', async () => {
    useMongoParser();
    const onParsed = vi.fn();
    const user = userEvent.setup();
    renderWithProviders(<UriPaste id="u" onParsed={onParsed} />);
    await user.type(screen.getByLabelText('Connection URI'), 'http://nope');
    await user.click(screen.getByRole('button', { name: 'Fill from URI' }));
    expect(await screen.findByRole('alert')).toBeInTheDocument();
    expect(onParsed).not.toHaveBeenCalled();
  });
});

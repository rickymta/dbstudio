import { screen, within } from '@testing-library/react';
import { beforeAll, describe, expect, it } from 'vitest';
import { StructureTab } from './StructureTab';
import { openMockSession, pgPath, renderWithProviders } from './testUtils';

let sessionId: string;
beforeAll(async () => {
  sessionId = await openMockSession();
});

describe('StructureTab (mock IPC)', () => {
  it('renders columns, indexes, foreign keys and the summary of a table', async () => {
    renderWithProviders(<StructureTab sessionId={sessionId} path={pgPath('orders')} />);
    const columns = await screen.findByRole('table', { name: 'Columns' });
    expect(within(columns).getAllByRole('row').length).toBeGreaterThan(3);
    expect(within(columns).getByText('id')).toBeInTheDocument();
    expect(within(columns).getAllByText('bigint').length).toBeGreaterThan(0);

    expect(screen.getByRole('table', { name: 'Indexes' })).toBeInTheDocument();
    const fks = screen.getByRole('table', { name: 'Foreign keys' });
    expect(within(fks).getAllByRole('row').length).toBeGreaterThan(1);

    const summary = screen.getByTestId('structure-summary');
    expect(summary).toHaveTextContent('Table');
    expect(summary).toHaveTextContent('Estimated rows');
    expect(summary).toHaveTextContent(/Size: .*(KB|MB|GB)/);
  });

  it('formats the estimated rows and size of the 1M-row table', async () => {
    renderWithProviders(<StructureTab sessionId={sessionId} path={pgPath('events')} />);
    const summary = await screen.findByTestId('structure-summary');
    expect(summary).toHaveTextContent('~1,000,000');
    expect(summary).toHaveTextContent('91.6 MB');
  });

  it('hides the index section for a view', async () => {
    renderWithProviders(
      <StructureTab sessionId={sessionId} path={pgPath('active_users', 'view')} />,
    );
    await screen.findByRole('table', { name: 'Columns' });
    expect(screen.getByTestId('structure-summary')).toHaveTextContent('View');
    expect(screen.queryByRole('table', { name: 'Indexes' })).toBeNull();
  });

  it('shows an error with retry for an unknown object', async () => {
    renderWithProviders(<StructureTab sessionId={sessionId} path={pgPath('nope')} />);
    expect(await screen.findByText('Could not load the structure')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Retry' })).toBeInTheDocument();
  });
});

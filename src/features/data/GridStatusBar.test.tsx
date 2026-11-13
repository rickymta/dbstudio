import { act, screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { SessionInfo } from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { GridModel } from './gridModel';
import { GridStatusBar } from './GridStatusBar';
import { renderWithProviders } from './testUtils';

afterEach(() => useSessionsStore.setState({ sessionsByConnection: {} }));

function session(cancelQuery: boolean): SessionInfo {
  return {
    sessionId: 's1',
    connectionId: 'c1',
    serverInfo: { product: 'x', version: '1', extra: {} },
    descriptor: { capabilities: { cancelQuery } },
  } as unknown as SessionInfo;
}

function loadingModel() {
  const m = new GridModel({ sessionId: 's1', table: { segments: [] } });
  m.status = 'loading';
  return m;
}

describe('GridStatusBar count', () => {
  it('shows a spinner and a cancel button while counting (only when cancellable)', () => {
    act(() => useSessionsStore.setState({ sessionsByConnection: { c1: session(true) } }));
    const m = new GridModel({ sessionId: 's1', table: { segments: [] } });
    m.status = 'ready';
    m.counting = true;
    m.cancelCount = vi.fn();
    const { unmount } = renderWithProviders(<GridStatusBar model={m} />);
    expect(screen.getByText('Counting...')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Count' })).toBeNull();
    screen.getByRole('button', { name: 'Cancel' }).click();
    expect(m.cancelCount).toHaveBeenCalled();
    unmount();
    act(() => useSessionsStore.setState({ sessionsByConnection: { c1: session(false) } }));
    renderWithProviders(<GridStatusBar model={m} />);
    expect(screen.queryByRole('button', { name: 'Cancel' })).toBeNull();
  });

  it('shows a count error compactly with the message in a tooltip', () => {
    const m = new GridModel({ sessionId: 's1', table: { segments: [] } });
    m.status = 'ready';
    m.countError = { kind: 'query', message: 'permission denied' };
    renderWithProviders(<GridStatusBar model={m} />);
    const el = screen.getByText('Count failed');
    expect(el).toHaveAttribute('title', expect.stringContaining('permission denied'));
  });
});

describe('GridStatusBar Stop button', () => {
  it('is shown only when the driver can cancel queries', () => {
    act(() => useSessionsStore.setState({ sessionsByConnection: { c1: session(true) } }));
    const { unmount } = renderWithProviders(<GridStatusBar model={loadingModel()} />);
    expect(screen.getByRole('button', { name: /stop/i })).toBeInTheDocument();
    unmount();
    act(() => useSessionsStore.setState({ sessionsByConnection: { c1: session(false) } }));
    renderWithProviders(<GridStatusBar model={loadingModel()} />);
    expect(screen.queryByRole('button', { name: /stop/i })).toBeNull();
  });

  it('explains a cell-budget stop', () => {
    const m = new GridModel({ sessionId: 's1', table: { segments: [] } });
    m.status = 'ready';
    m.capped = true;
    m.capReason = 'cells';
    m.columns = [{ name: 'a', typeName: 'int', valueKind: 'int' }];
    renderWithProviders(<GridStatusBar model={m} />);
    expect(screen.getByRole('status')).toHaveTextContent(/Settings/);
  });
});

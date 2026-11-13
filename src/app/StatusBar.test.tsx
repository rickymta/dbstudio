import { render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it } from 'vitest';
import i18n from '@/lib/i18n';
import { ROOT_PATH } from '@/lib/objectPath';
import { useTabsStore, WELCOME_TAB, type TabItem } from '@/stores/tabs';
import { Providers } from './providers';
import { StatusBar } from './StatusBar';

const PROD = '00000000-0000-4000-8000-0000000000b3';
const DEV = '00000000-0000-4000-8000-0000000000b1';

const tableTab = (connectionId: string): TabItem => ({
  id: `table:${connectionId}`,
  kind: 'table',
  title: 'users',
  closable: true,
  payload: { sessionId: 's', connectionId, path: ROOT_PATH },
});

beforeEach(async () => {
  await i18n.changeLanguage('en');
  useTabsStore.setState({ tabs: [WELCOME_TAB], activeId: WELCOME_TAB.id });
});

const renderBar = () =>
  render(
    <Providers>
      <StatusBar />
    </Providers>,
  );

describe('StatusBar active connection', () => {
  it('is hidden when the active tab has no connection', () => {
    renderBar();
    expect(screen.queryByTestId('status-connection')).toBeNull();
  });

  it('shows name, environment, color and read-only lock of the active tab', async () => {
    useTabsStore.setState({ tabs: [WELCOME_TAB, tableTab(PROD)], activeId: `table:${PROD}` });
    renderBar();
    const box = await screen.findByTestId('status-connection');
    expect(box).toHaveTextContent('PostgreSQL prod (read-only)');
    expect(box).toHaveTextContent('Production');
    expect(screen.getByLabelText('Read-only')).toBeInTheDocument();
    expect(screen.getByTestId('status-connection-color')).toHaveStyle({
      backgroundColor: '#e5484d',
    });
  });

  it('follows the active tab and omits the lock for writable connections', async () => {
    useTabsStore.setState({
      tabs: [WELCOME_TAB, tableTab(PROD), tableTab(DEV)],
      activeId: `table:${DEV}`,
    });
    renderBar();
    const box = await screen.findByTestId('status-connection');
    expect(box).toHaveTextContent('PostgreSQL dev');
    expect(box).toHaveTextContent('Development');
    expect(screen.queryByLabelText('Read-only')).toBeNull();
  });
});

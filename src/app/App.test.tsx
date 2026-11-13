import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import i18n, { setLanguage } from '@/lib/i18n';
import { App } from './App';
import { Providers } from './providers';

function renderApp() {
  return render(
    <Providers>
      <App />
    </Providers>,
  );
}

describe('App shell (browser mock IPC)', () => {
  beforeEach(async () => {
    localStorage.clear();
    await i18n.changeLanguage('en');
  });

  it('renders the sidebar empty state and all drivers from list_drivers', async () => {
    renderApp();
    expect(screen.getByText('Connections')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /new connection/i })).toBeDisabled();
    expect(await screen.findByTestId('driver-postgres')).toHaveTextContent('Ready');
    expect(screen.getByTestId('driver-clickhouse')).toHaveTextContent('Phase 10');
    expect(screen.getAllByRole('listitem')).toHaveLength(9);
  });

  it('shows app version in the status bar', async () => {
    renderApp();
    await waitFor(() =>
      expect(screen.getByTestId('app-version')).toHaveTextContent('DBStudio v0.1.0'),
    );
  });

  it('switches language to Vietnamese and persists it', async () => {
    const user = userEvent.setup();
    renderApp();
    await user.click(screen.getByRole('button', { name: /language/i }));
    expect(await screen.findByText('Kết nối')).toBeInTheDocument();
    expect(localStorage.getItem('dbstudio.language')).toBe('vi');
    setLanguage('en');
  });

  it('cycles theme and toggles the dark class', async () => {
    const user = userEvent.setup();
    renderApp();
    const btn = () => screen.getByRole('button', { name: /theme/i });
    await user.click(btn()); // system -> light
    await user.click(btn()); // light -> dark
    expect(document.documentElement).toHaveClass('dark');
    expect(localStorage.getItem('dbstudio.theme')).toBe('dark');
  });
});

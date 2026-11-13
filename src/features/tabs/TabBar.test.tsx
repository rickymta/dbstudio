import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { Providers } from '@/app/providers';
import { ROOT_PATH } from '@/lib/objectPath';
import { useTabsStore, WELCOME_TAB, type TabItem } from '@/stores/tabs';
import { TabBar } from './TabBar';

const tab = (name: string): TabItem => ({
  id: `table:${name}`,
  kind: 'table',
  title: name,
  closable: true,
  payload: { sessionId: 's', connectionId: 'c', path: ROOT_PATH },
});

beforeEach(() => {
  useTabsStore.setState({ tabs: [WELCOME_TAB, tab('a'), tab('b')], activeId: 'welcome' });
});

function setup() {
  render(
    <Providers>
      <TabBar />
    </Providers>,
  );
  return userEvent.setup();
}

describe('TabBar keyboard support', () => {
  it('uses a roving tabindex', () => {
    setup();
    const tabs = screen.getAllByRole('tab');
    expect(tabs.map((t) => t.getAttribute('tabindex'))).toEqual(['0', '-1', '-1']);
  });

  it('moves between tabs with arrows, Home and End, wrapping around', async () => {
    const user = setup();
    screen.getByRole('tab', { name: 'Welcome' }).focus();
    await user.keyboard('{ArrowRight}');
    expect(useTabsStore.getState().activeId).toBe('table:a');
    expect(screen.getByRole('tab', { name: 'a' })).toHaveFocus();
    await user.keyboard('{End}');
    expect(useTabsStore.getState().activeId).toBe('table:b');
    await user.keyboard('{ArrowRight}');
    expect(useTabsStore.getState().activeId).toBe('welcome');
    await user.keyboard('{ArrowLeft}');
    expect(useTabsStore.getState().activeId).toBe('table:b');
    await user.keyboard('{Home}');
    expect(useTabsStore.getState().activeId).toBe('welcome');
  });

  it('closes the focused tab with Delete or Ctrl+W, except non-closable ones', async () => {
    const user = setup();
    const welcome = screen.getByRole('tab', { name: 'Welcome' });
    welcome.focus();
    await user.keyboard('{Delete}');
    expect(useTabsStore.getState().tabs).toHaveLength(3);

    await user.keyboard('{ArrowRight}');
    await user.keyboard('{Delete}');
    expect(useTabsStore.getState().tabs.map((t) => t.id)).toEqual(['welcome', 'table:b']);
    await waitFor(() => expect(screen.getByRole('tab', { name: 'Welcome' })).toHaveFocus());

    await user.keyboard('{ArrowRight}{Control>}w{/Control}');
    expect(useTabsStore.getState().tabs.map((t) => t.id)).toEqual(['welcome']);
  });
});

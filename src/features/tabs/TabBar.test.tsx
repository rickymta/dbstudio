import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { dialogs } from '@/components/ui/confirm';
import { Providers } from '@/app/providers';
import { registerEditorTab } from '@/features/editor/editorRunner';
import { newTabData, useEditorStore } from '@/features/editor/editorStore';
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

describe('TabBar unsaved file', () => {
  const editorTab: TabItem = {
    id: 'editor:e1',
    kind: 'editor',
    title: 'report.sql',
    closable: true,
    payload: { tabStateId: 'e1', connectionId: null, sessionId: null, mode: 'sql' },
  };

  const mount = (text: string) => {
    useEditorStore.setState({
      tabs: { e1: newTabData('e1', { text, filePath: '/work/report.sql', savedText: 'select 1' }) },
    });
    registerEditorTab('e1');
    useTabsStore.setState({ tabs: [WELCOME_TAB, editorTab], activeId: 'editor:e1' });
    return setup();
  };

  afterEach(() => {
    vi.restoreAllMocks();
    useEditorStore.setState({ tabs: {} });
  });

  it('marks the tab of a changed file and asks before closing it', async () => {
    const confirm = vi.spyOn(dialogs, 'confirm').mockResolvedValue(false);
    const user = mount('select 2');
    expect(screen.getByTestId('tab-unsaved')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: /^Close tab/ }));
    expect(confirm).toHaveBeenCalledWith(
      expect.objectContaining({ description: expect.stringContaining('report.sql') }),
    );
    expect(useTabsStore.getState().tabs).toHaveLength(2);
    confirm.mockResolvedValue(true);
    await user.click(screen.getByRole('button', { name: /^Close tab/ }));
    await waitFor(() => expect(useTabsStore.getState().tabs).toHaveLength(1));
  });

  it('closes a saved file without asking', async () => {
    const confirm = vi.spyOn(dialogs, 'confirm');
    const user = mount('select 1');
    expect(screen.queryByTestId('tab-unsaved')).toBeNull();
    await user.click(screen.getByRole('button', { name: /^Close tab/ }));
    await waitFor(() => expect(useTabsStore.getState().tabs).toHaveLength(1));
    expect(confirm).not.toHaveBeenCalled();
  });
});

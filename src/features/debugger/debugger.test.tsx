import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { Providers } from '@/app/providers';
import { polyfillLayoutApis } from '@/features/data/testUtils';
import { EditorHost } from '@/features/editor/EditorHost';
import { EditorTab } from '@/features/editor/EditorTab';
import { getTabData } from '@/features/editor/editorStore';
import { openEditor } from '@/features/editor/open';
import { openSession, resetEditorWorld } from '@/features/editor/testUtils';
import { ConfirmDestructiveHost } from '@/features/safety/ConfirmDestructiveHost';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import { mockEmit } from '@/lib/mock/events';
import { resetMockDebug } from '@/lib/mock/debug';
import type { DebugOutputEvent, DebugState } from '@/lib/types';
import { editorTabId, useTabsStore } from '@/stores/tabs';
import { viewOf } from './cm/viewRegistry';
import { debugStateOf, applyDebugState } from '@/features/editor/persistHooks';
import DebugSourceTab from './DebugSourceTab';
import { resetDebugActions, runCommand, toggleBreakpoint } from './debugActions';
import { resetDebugStore, scriptScope, sessionOf, useDebugStore } from './debugStore';
import { resetDebuggerExtensions } from './extensions';
import { useDebugUi } from './debugUiStore';

beforeAll(polyfillLayoutApis);

const TEXT = [
  'select 1;',
  'select * from users;',
  "update users set name = 'x' where id = 1;",
].join('\n');

beforeEach(async () => {
  await i18n.changeLanguage('en');
  resetDebugActions();
  resetEditorWorld();
  resetMockDebug();
  resetDebugStore();
  resetDebuggerExtensions();
  useDebugUi.setState({ dialogs: {}, popover: null });
  vi.restoreAllMocks();
});

async function mountEditor() {
  const session = await openSession();
  const id = openEditor(session, { text: TEXT });
  const tab = useTabsStore.getState().tabs.find((t) => t.id === editorTabId(id))!;
  if (tab.kind !== 'editor') throw new Error('not an editor tab');
  render(
    <Providers>
      <EditorTab {...tab.payload} />
      <EditorHost />
      <ConfirmDestructiveHost />
    </Providers>,
  );
  await screen.findByTestId('sql-editor', undefined, { timeout: 5000 });
  return { id };
}

async function startSession(id: string) {
  await userEvent.click(screen.getByRole('button', { name: 'Debug' }));
  await screen.findByTestId('debug-tool-window');
  await waitFor(() => expect(sessionOf(id)).toBeTruthy());
}

/** jsdom has no layout: the panel resizer would catch the click `type` does first, so skip it. */
async function typeInto(el: HTMLElement, text: string): Promise<void> {
  act(() => el.focus());
  await userEvent.type(el, text, { skipClick: true });
}

const bpMarkers = () => document.querySelectorAll<HTMLElement>('.cm-bp');

describe('breakpoint gutter', () => {
  it('toggles breakpoints by clicking the gutter and mirrors them in the store', async () => {
    const { id } = await mountEditor();
    const gutter = document.querySelector('.cm-bp-gutter')!;
    const cell = gutter.querySelectorAll('.cm-gutterElement')[1] as HTMLElement;
    fireEvent.mouseDown(cell);
    const list = () => useDebugStore.getState().scopes[scriptScope(id)]!.breakpoints;
    await waitFor(() => expect(list()).toHaveLength(1));
    await waitFor(() => expect(bpMarkers()).toHaveLength(1));
    expect(bpMarkers()[0]!.getAttribute('aria-label')).toMatch(/^Breakpoint line \d+$/);
    fireEvent.mouseDown(gutter.querySelectorAll('.cm-gutterElement')[1] as HTMLElement);
    await waitFor(() => expect(list()).toHaveLength(0));
    await waitFor(() => expect(bpMarkers()).toHaveLength(0));
  });

  it('Alt+click adds a disabled breakpoint and a condition changes its shape', async () => {
    const { id } = await mountEditor();
    const cell = document.querySelectorAll('.cm-bp-gutter .cm-gutterElement')[1] as HTMLElement;
    fireEvent.mouseDown(cell, { altKey: true });
    await waitFor(() => expect(document.querySelector('.cm-bp-off')).toBeTruthy());
    const bp = useDebugStore.getState().scopes[scriptScope(id)]!.breakpoints[0]!;
    expect(bp.enabled).toBe(false);
    act(() => {
      useDebugStore
        .getState()
        .setBreakpoints(scriptScope(id), [{ ...bp, enabled: true, condition: 'id = 5' }]);
    });
    await waitFor(() => expect(document.querySelector('.cm-bp-cond')).toBeTruthy());
  });

  it('right click opens the popover where condition, log message and hit count are edited', async () => {
    const { id } = await mountEditor();
    const cell = document.querySelectorAll('.cm-bp-gutter .cm-gutterElement')[1] as HTMLElement;
    fireEvent.contextMenu(cell);
    const popover = await screen.findByRole('dialog', { name: /Breakpoint at line/ });
    const cond = within(popover).getByLabelText('Condition');
    await typeInto(cond, 'id = 5');
    await typeInto(within(popover).getByLabelText('Log message'), 'hit {{id}');
    await userEvent.clear(within(popover).getByLabelText('Stop on the N-th hit'));
    await typeInto(within(popover).getByLabelText('Stop on the N-th hit'), '3');
    await userEvent.tab();
    const bp = useDebugStore.getState().scopes[scriptScope(id)]!.breakpoints[0]!;
    expect(bp).toMatchObject({ condition: 'id = 5', logMessage: 'hit {id}', hitCount: 3 });
    await userEvent.click(within(popover).getByRole('button', { name: 'Remove' }));
    expect(useDebugStore.getState().scopes[scriptScope(id)]!.breakpoints).toHaveLength(0);
  });

  it('Mod+F8 toggles a breakpoint at the cursor without a session', async () => {
    const { id } = await mountEditor();
    const content = document.querySelector('.cm-content')!;
    fireEvent.keyDown(content, { key: 'F8', code: 'F8', ctrlKey: true });
    await waitFor(() =>
      expect(useDebugStore.getState().scopes[scriptScope(id)]!.breakpoints).toHaveLength(1),
    );
  });
});

describe('debug session', () => {
  it('starts paused at entry, shows the panels and sends the breakpoints', async () => {
    const { id } = await mountEditor();
    const start = vi.spyOn(ipc, 'debugStart');
    toggleBreakpoint(scriptScope(id), 2);
    await startSession(id);
    expect(start.mock.calls[0]![0]).toMatchObject({
      text: TEXT,
      breakpoints: [{ line: 2, enabled: true }],
    });
    expect(screen.getByTestId('debug-status')).toHaveTextContent(/Paused/);
    expect(screen.getByRole('tree', { name: 'Frames' })).toBeInTheDocument();
    expect(screen.getByRole('treegrid', { name: 'Variables' })).toBeInTheDocument();
    expect(screen.getByRole('tree', { name: 'Watches' })).toBeInTheDocument();
    expect(screen.getByRole('toolbar', { name: 'Debug session' })).toBeInTheDocument();
    // the editor is read only while debugging
    await waitFor(() => expect(document.querySelector('.cm-exec-line')).toBeTruthy());
    expect(screen.getByRole('button', { name: /^Step Over \(F8\)/ })).toBeEnabled();
    expect(screen.getByRole('button', { name: /^Run to Cursor/ })).toBeEnabled();
  });

  it.each([
    ['jetBrains', 'F8', {}, 'stepOver'],
    ['jetBrains', 'F7', {}, 'stepInto'],
    ['jetBrains', 'F9', {}, 'resume'],
    ['jetBrains', 'F9', { altKey: true }, 'runToCursor'],
    ['jetBrains', 'F8', { shiftKey: true }, 'stepOut'],
    ['vsCode', 'F10', {}, 'stepOver'],
    ['vsCode', 'F11', {}, 'stepInto'],
  ] as const)('%s key %s calls %s', async (keymap, key, mods, expected) => {
    const { id } = await mountEditor();
    await startSession(id);
    act(() => useDebugStore.getState().setKeymap(keymap));
    // Pretend the backend supports every step so the commands are enabled.
    const session = sessionOf(id)!;
    act(() =>
      useDebugStore.getState().patchSession(id, {
        state: {
          ...session.state,
          caps: { ...session.state.caps, stepInto: true, stepOut: true },
        },
      }),
    );
    const step = vi.spyOn(ipc, 'debugStep');
    const resume = vi.spyOn(ipc, 'debugResume');
    const runTo = vi.spyOn(ipc, 'debugRunToCursor');
    const toolWindow = screen.getByTestId('debug-tool-window');
    fireEvent.keyDown(toolWindow, { key, code: key, ...mods });
    await waitFor(() => {
      if (expected === 'resume') {
        expect(resume.mock.calls.length + step.mock.calls.length).toBeGreaterThan(0);
      } else if (expected === 'runToCursor') {
        expect(step.mock.calls.some((c) => c[1].kind === 'runToCursor')).toBe(true);
      } else {
        expect(step.mock.calls.some((c) => c[1].kind === expected)).toBe(true);
      }
    });
    void runTo;
  });

  it('keys also work with the focus in the editor', async () => {
    const { id } = await mountEditor();
    await startSession(id);
    const step = vi.spyOn(ipc, 'debugStep');
    fireEvent.keyDown(document.querySelector('.cm-content')!, { key: 'F8', code: 'F8' });
    await waitFor(() =>
      expect(step).toHaveBeenCalledWith(expect.any(String), { kind: 'stepOver' }),
    );
    void id;
  });

  it('stopping a session that owns a transaction asks Commit or Rollback (Rollback is the default)', async () => {
    const { id } = await mountEditor();
    await startSession(id);
    const session = sessionOf(id)!;
    // the debugger owns an open transaction
    act(() =>
      useDebugStore.getState().patchSession(id, {
        state: { ...session.state, transaction: { ...session.state.transaction, owned: true } },
      }),
    );
    const stop = vi.spyOn(ipc, 'debugStop');
    await userEvent.click(screen.getByRole('button', { name: /^Stop \(/ }));
    const dialog = await screen.findByRole('dialog', { name: 'End the debug transaction' });
    expect(within(dialog).getByRole('button', { name: 'Rollback' })).toHaveFocus();
    expect(stop).not.toHaveBeenCalled();
    await userEvent.click(within(dialog).getByRole('button', { name: 'Rollback' }));
    await waitFor(() => expect(stop).toHaveBeenCalledWith(session.debugId, 'rollback'));
    await waitFor(() => expect(screen.getByTestId('debug-status')).toHaveTextContent(/Stopped/));
    await userEvent.click(
      within(screen.getByTestId('debug-report').parentElement!).getByRole('button', {
        name: 'Close',
      }),
    );
    await waitFor(() => expect(sessionOf(id)).toBeUndefined());
  });

  it('closing the editor tab stops the session with Rollback', async () => {
    const { id } = await mountEditor();
    await startSession(id);
    const debugId = sessionOf(id)!.debugId;
    const stop = vi.spyOn(ipc, 'debugStop');
    act(() => useTabsStore.getState().closeTab(editorTabId(id)));
    await waitFor(() => expect(stop).toHaveBeenCalledWith(debugId, 'rollback'));
    expect(sessionOf(id)).toBeUndefined();
    void getTabData;
  });
});

function emitState(id: string, patch: Partial<DebugState>) {
  const session = sessionOf(id)!;
  const state: DebugState = { ...session.state, ...patch, seq: session.state.seq + 1 };
  act(() => mockEmit('debug://state', state));
  return state;
}

describe('panels', () => {
  it('Variables render the payload, mark changed values and load children lazily', async () => {
    const { id } = await mountEditor();
    await startSession(id);
    const children = vi.spyOn(ipc, 'debugVariableChildren').mockResolvedValue([
      {
        reference: 'p1',
        name: ':id',
        kind: 'parameter',
        display: '5',
        typeName: 'int4',
        editable: true,
        hasChildren: false,
        changed: true,
      },
    ]);
    emitState(id, {
      variables: [
        {
          reference: 'g1',
          name: 'Parameters',
          kind: 'group',
          display: '1',
          editable: false,
          hasChildren: true,
          changed: false,
        },
      ],
    });
    const grid = await screen.findByRole('treegrid', { name: 'Variables' });
    const group = await within(grid).findByText('Parameters');
    await userEvent.click(within(grid).getByRole('button', { name: 'Expand' }));
    expect(children).toHaveBeenCalled();
    const row = await within(grid).findByText(':id');
    expect(row.closest('[role=row]')).toBeTruthy();
    expect(within(grid).getByText('{int4}')).toBeInTheDocument();
    expect(grid.querySelector('[data-changed]')).toBeTruthy();
    void group;
  });

  it('Set Value of an editable variable calls debug_set_variable', async () => {
    const { id } = await mountEditor();
    await startSession(id);
    emitState(id, {
      variables: [
        {
          reference: 'p1',
          name: ':id',
          kind: 'parameter',
          display: '5',
          editable: true,
          hasChildren: false,
          changed: false,
        },
      ],
    });
    const set = vi.spyOn(ipc, 'debugSetVariable').mockResolvedValue({
      reference: 'p1',
      name: ':id',
      kind: 'parameter',
      display: '7',
      editable: true,
      hasChildren: false,
      changed: true,
    });
    const row = (await screen.findByText(':id')).closest('[role=row]') as HTMLElement;
    fireEvent.keyDown(row, { key: 'F2' });
    const input = await screen.findByLabelText('New value of :id');
    await userEvent.clear(input);
    await typeInto(input, '7{Enter}');
    await waitFor(() => expect(set).toHaveBeenCalledWith(expect.any(String), 'p1', '7'));
  });

  it('Watches can be added, edited and removed; results and errors show', async () => {
    const { id } = await mountEditor();
    await startSession(id);
    const set = vi.spyOn(ipc, 'debugWatchesSet').mockImplementation(async (_d, watches) =>
      watches.map((w) => ({
        id: w.id,
        expr: w.expr,
        result: {
          context: 'lease' as const,
          elapsedMs: 1,
          value: w.expr.includes('bad')
            ? { kind: 'error' as const, error: { kind: 'query', message: 'no such column' } }
            : { kind: 'scalar' as const, value: { t: 'int' as const, v: 42 }, typeName: 'int4' },
        },
      })),
    );
    await userEvent.click(screen.getByRole('button', { name: 'Add watch' }));
    await typeInto(screen.getByLabelText('New watch expression'), 'count(*){Enter}');
    await waitFor(() => expect(set).toHaveBeenCalled());
    const watches = screen.getByRole('tree', { name: 'Watches' });
    await within(watches).findByText('count(*)');
    // results come with the next state
    const specs = useDebugStore.getState().watches[id]!;
    expect(specs.map((w) => w.expr)).toEqual(['count(*)']);
    emitState(id, {
      watches: [
        {
          id: specs[0]!.id,
          expr: 'count(*)',
          result: {
            context: 'lease',
            elapsedMs: 1,
            value: { kind: 'scalar', value: { t: 'int', v: 42 }, typeName: 'int4' },
          },
        },
      ],
    });
    expect(await within(watches).findByText('42')).toBeInTheDocument();
    // edit
    fireEvent.doubleClick(within(watches).getByText('count(*)'));
    const input = screen.getByLabelText('Edit watch');
    await userEvent.clear(input);
    await typeInto(input, 'bad_col{Enter}');
    await waitFor(() =>
      expect(useDebugStore.getState().watches[id]!.map((w) => w.expr)).toEqual(['bad_col']),
    );
    // remove
    await userEvent.click(within(watches).getByText('bad_col'));
    await userEvent.click(screen.getByRole('button', { name: 'Remove watch' }));
    await waitFor(() => expect(useDebugStore.getState().watches[id]).toEqual([]));
  });

  it('Console shows messages, result grids, errors and evaluates typed expressions', async () => {
    const { id } = await mountEditor();
    await startSession(id);
    const debugId = sessionOf(id)!.debugId;
    let seq = sessionOf(id)!.state.seq;
    const out = (output: DebugOutputEvent['output']) =>
      act(() => mockEmit('debug://output', { debugId, seq: ++seq, output }));
    out({ kind: 'stepStart', step: 1, line: 2, text: 'select * from users' });
    out({
      kind: 'query',
      step: 1,
      event: {
        type: 'columns',
        columns: Array.from({ length: 2 }, (_, i) => ({
          name: i === 0 ? 'id' : 'name',
          typeName: 'text',
          valueKind: 'text' as const,
        })),
      },
    });
    out({
      kind: 'query',
      step: 1,
      event: {
        type: 'rows',
        rows: Array.from({ length: 8 }, (_, i) => [
          { t: 'int' as const, v: i },
          { t: 'text' as const, v: `user${i}` },
        ]),
      },
    });
    out({
      kind: 'stepEnd',
      step: 1,
      end: { outcome: { kind: 'rows', rowCount: 8, truncated: false }, elapsedMs: 3 },
    });
    out({ kind: 'message', text: 'relation created', source: 'notice' });
    out({ kind: 'error', step: 2, error: { kind: 'query', message: 'syntax error near x' } });
    await userEvent.click(screen.getByRole('tab', { name: 'Console' }));
    const log = await screen.findByRole('log', { name: 'Console' });
    expect(within(log).getByText('select * from users')).toBeInTheDocument();
    const grid = within(log).getByTestId('mini-grid');
    // collapsed to 5 rows, expandable
    expect(within(grid).getAllByRole('row')).toHaveLength(1 + 5);
    await userEvent.click(within(grid).getByRole('button', { name: /Expand/ }));
    expect(within(grid).getAllByRole('row')).toHaveLength(1 + 8);
    expect(within(log).getByText('relation created')).toBeInTheDocument();
    expect(within(log).getByText('syntax error near x')).toBeInTheDocument();
    // evaluate at the prompt, history with the arrow keys
    vi.spyOn(ipc, 'debugEvaluate').mockResolvedValue({
      context: 'lease',
      elapsedMs: 1,
      value: { kind: 'scalar', value: { t: 'int', v: 99 }, typeName: 'int4' },
    });
    const prompt = screen.getByLabelText('Evaluate an expression in the paused context');
    await typeInto(prompt, '1+1{Enter}');
    expect(await within(log).findByText('99')).toBeInTheDocument();
    await typeInto(prompt, '{ArrowUp}');
    expect(prompt).toHaveValue('1+1');
    // filter out errors
    await userEvent.click(screen.getByRole('button', { name: 'Errors' }));
    expect(within(log).queryByText('syntax error near x')).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Clear console' }));
    expect(within(log).queryByText('relation created')).not.toBeInTheDocument();
  });

  it('shows the Evaluate dialog result and can add it to the watches', async () => {
    const { id } = await mountEditor();
    await startSession(id);
    vi.spyOn(ipc, 'debugEvaluate').mockResolvedValue({
      context: 'lease',
      elapsedMs: 1,
      value: { kind: 'scalar', value: { t: 'text', v: 'hello' }, typeName: 'text' },
    });
    act(() => runCommand(id, 'evaluate'));
    const dialog = await screen.findByRole('dialog', { name: 'Evaluate Expression' });
    await typeInto(within(dialog).getByLabelText('Expression'), "'hello'");
    await userEvent.click(within(dialog).getByRole('button', { name: 'Evaluate' }));
    expect(await within(dialog).findByText('hello')).toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole('button', { name: 'Add to Watches' }));
    expect(useDebugStore.getState().watches[id]!.map((w) => w.expr)).toEqual(["'hello'"]);
  });

  it('Frames select a frame; the transaction warning offers Rollback & stop', async () => {
    const { id } = await mountEditor();
    await startSession(id);
    const base = sessionOf(id)!.state;
    emitState(id, {
      frames: [
        { id: 0, kind: 'routine', name: 'f_inner', source: { kind: 'script' }, line: 3 },
        { id: 1, kind: 'script', name: 'script.sql', source: { kind: 'script' }, line: 1 },
      ],
      transaction: { ...base.transaction, owned: true, warn: true, openForMs: 83000 },
    });
    const select = vi.spyOn(ipc, 'debugSelectFrame');
    const frames = screen.getByRole('tree', { name: 'Frames' });
    await userEvent.click(within(frames).getByText('script.sql'));
    expect(select).toHaveBeenCalledWith(base.debugId, 1);
    expect(await screen.findByText(/open for 1m 23s/)).toBeInTheDocument();
    const stop = vi.spyOn(ipc, 'debugStop');
    await userEvent.click(screen.getByRole('button', { name: 'Roll back and stop' }));
    await waitFor(() => expect(stop).toHaveBeenCalledWith(base.debugId, 'rollback'));
  });

  it('asks for the typed confirmation when a statement needs it', async () => {
    const { id } = await mountEditor();
    await startSession(id);
    emitState(id, {
      status: {
        kind: 'paused',
        reason: {
          kind: 'confirmation',
          request: {
            token: 'tok-1',
            confirmText: 'DELETE',
            statements: [],
            expiresInMs: 60000,
          },
        },
      },
    });
    expect(await screen.findByText(/needs your confirmation/)).toBeInTheDocument();
    expect(await screen.findByRole('dialog')).toBeInTheDocument();
  });
});

describe('execution line and inline values', () => {
  it('highlights the paused line and draws inline values at the end of the line', async () => {
    const { id } = await mountEditor();
    await startSession(id);
    emitState(id, {
      inlineValues: [{ source: { kind: 'script' }, line: 1, text: '1 row: n=1', kind: 'result' }],
    });
    await waitFor(() => expect(document.querySelector('.cm-inline-value')).toBeTruthy());
    expect(document.querySelector('.cm-inline-value')!.textContent).toBe('1 row: n=1');
    expect(document.querySelector('.cm-exec-arrow')).toBeTruthy();
    // switching inline values off removes them
    act(() => useDebugStore.getState().setInlineValues(false));
    await waitFor(() => expect(document.querySelector('.cm-inline-value')).toBeNull());
  });
});

describe('routine source tab', () => {
  it('opens when Step Into enters a routine, marks the execution line and saves breakpoints', async () => {
    const session = await openSession();
    const id = openEditor(session, { text: 'select f_outer(1);\nselect 2;' });
    const tab = useTabsStore.getState().tabs.find((t) => t.id === editorTabId(id))!;
    if (tab.kind !== 'editor') throw new Error('not an editor tab');
    render(
      <Providers>
        <EditorTab {...tab.payload} />
        <EditorHost />
      </Providers>,
    );
    await screen.findByTestId('sql-editor', undefined, { timeout: 5000 });
    await startSession(id);
    await userEvent.click(screen.getByRole('button', { name: /^Step Into/ }));
    await waitFor(() =>
      expect(useTabsStore.getState().tabs.some((t) => t.kind === 'debugSource')).toBe(true),
    );
    const source = useTabsStore.getState().tabs.find((t) => t.kind === 'debugSource')!;
    if (source.kind !== 'debugSource') throw new Error('unreachable');
    expect(source.title).toBe('f_outer() [debug]');
    expect(useTabsStore.getState().tabs.filter((t) => t.kind === 'debugSource')).toHaveLength(1);

    render(
      <Providers>
        <DebugSourceTab {...source.payload} />
      </Providers>,
    );
    const editor = await screen.findByTestId('debug-source-editor');
    await waitFor(() =>
      expect(editor.querySelector('.cm-content')?.textContent).toContain('p_n * 2'),
    );
    await waitFor(() => expect(editor.querySelector('.cm-exec-line')).toBeTruthy());

    const save = vi.spyOn(ipc, 'routineBreakpointsSet');
    fireEvent.mouseDown(editor.querySelector('.cm-bp-gutter') as HTMLElement);
    await waitFor(() => expect(save).toHaveBeenCalled());
    const [connectionId, list] = save.mock.calls[0]!;
    expect(connectionId).toBe(source.payload.connectionId);
    expect(list).toHaveLength(1);
    expect(list[0]!.routine.signature).toBe(source.payload.signature);
  });

  async function intoRoutine() {
    const session = await openSession();
    const id = openEditor(session, { text: 'select f_outer(1);\nselect 2;' });
    const tab = useTabsStore.getState().tabs.find((t) => t.id === editorTabId(id))!;
    if (tab.kind !== 'editor') throw new Error('not an editor tab');
    render(
      <Providers>
        <EditorTab {...tab.payload} />
        <EditorHost />
      </Providers>,
    );
    await screen.findByTestId('sql-editor', undefined, { timeout: 5000 });
    await startSession(id);
    await userEvent.click(screen.getByRole('button', { name: /^Step Into/ }));
    await waitFor(() =>
      expect(useTabsStore.getState().tabs.some((t) => t.kind === 'debugSource')).toBe(true),
    );
    const source = useTabsStore.getState().tabs.find((t) => t.kind === 'debugSource')!;
    if (source.kind !== 'debugSource') throw new Error('unreachable');
    return { id, source };
  }

  it('the routine tab shows the same Debug tool window; Routine variables come first and open', async () => {
    const { id, source } = await intoRoutine();
    const frames = sessionOf(id)!.state.frames;
    expect(frames[sessionOf(id)!.state.selectedFrame]!.kind).toBe('routine');
    const listeners = vi.spyOn(ipc, 'debugStart');
    render(
      <Providers>
        <DebugSourceTab {...source.payload} />
      </Providers>,
    );
    expect(listeners).not.toHaveBeenCalled();
    const windows = await screen.findAllByTestId('debug-tool-window');
    const win = windows[windows.length - 1]!;
    const grid = await within(win).findByRole('treegrid', { name: 'Variables' });
    const rows = () => within(grid).getAllByRole('row');
    expect(rows()[0]!.textContent).toMatch(/^Routine/);
    expect(rows()[0]).toHaveAttribute('aria-expanded', 'true');
    await waitFor(() => expect(rows().map((r) => r.textContent)).toContain('p_n=1{int4}'));
    expect(rows()[1]!.getAttribute('aria-level')).toBe('2');
    // the user's choice sticks across steps
    await userEvent.click(within(rows()[0]!).getByRole('button', { name: 'Collapse' }));
    emitState(id, {});
    await waitFor(() => expect(rows()[0]).toHaveAttribute('aria-expanded', 'false'));
  });

  it('Enter in the Variables prompt evaluates inline, Shift+Enter adds a watch', async () => {
    const { id } = await mountEditor();
    await startSession(id);
    const evaluate = vi.spyOn(ipc, 'debugEvaluate').mockResolvedValue({
      context: 'lease',
      elapsedMs: 1,
      value: { kind: 'scalar', value: { t: 'int', v: 6 }, typeName: 'integer' },
    });
    const input = screen.getByLabelText(/Evaluate expression/);
    await typeInto(input, 's{Enter}');
    const out = await screen.findByTestId('variables-inline-eval');
    await waitFor(() => expect(out.textContent).toContain('s ='));
    expect(out.textContent).toContain('6');
    expect(out.textContent).toContain('{integer}');
    expect(evaluate).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole('dialog', { name: 'Evaluate Expression' })).not.toBeInTheDocument();
    evaluate.mockResolvedValue({
      context: 'lease',
      elapsedMs: 1,
      value: { kind: 'error', error: { kind: 'sql', message: 'column "zz" does not exist' } },
    });
    await typeInto(input, 'zz{Enter}');
    await waitFor(() => expect(out.textContent).toContain('does not exist'));
    await typeInto(input, 'w{Shift>}{Enter}{/Shift}');
    await waitFor(() =>
      expect(useDebugStore.getState().watches[id]!.map((w) => w.expr)).toEqual(['w']),
    );
  });
});

describe('breakpoints follow the text and the tab state', () => {
  it('moves a breakpoint when lines are inserted above it', async () => {
    const { id } = await mountEditor();
    toggleBreakpoint(scriptScope(id), 2);
    await waitFor(() => expect(bpMarkers()).toHaveLength(1));
    const view = viewOf(scriptScope(id))!;
    act(() => view.dispatch({ changes: { from: 0, insert: '-- comment\n' } }));
    await waitFor(() =>
      expect(useDebugStore.getState().scopes[scriptScope(id)]!.breakpoints[0]!.line).toBe(3),
    );
  });

  it('saves and restores breakpoints, watches and mute through the persist hooks', async () => {
    const { id } = await mountEditor();
    expect(debugStateOf(id)).toBeNull();
    toggleBreakpoint(scriptScope(id), 1);
    useDebugStore.getState().setWatches(id, [{ id: 'w1', expr: 'x' }]);
    useDebugStore.getState().setMuted(id, true);
    const saved = debugStateOf(id)!;
    expect(saved).toMatchObject({ watches: [{ expr: 'x' }], muted: true });
    expect(saved.breakpoints).toHaveLength(1);
    resetDebugStore();
    applyDebugState(id, saved);
    expect(useDebugStore.getState().scopes[scriptScope(id)]!.breakpoints).toEqual(
      saved.breakpoints,
    );
    expect(useDebugStore.getState().muted[id]).toBe(true);
  });
});

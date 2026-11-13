import { CompletionContext } from '@codemirror/autocomplete';
import { EditorState } from '@codemirror/state';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ipc } from '@/lib/ipc';
import { resetCatalogCache } from './catalogLoader';
import { createCompletionSource } from './completionSource';
import { openSession, resetEditorWorld } from './testUtils';

beforeEach(() => {
  resetEditorWorld();
  resetCatalogCache();
  // The ERD mock answers `schema_model` with its own sample schema; these tests are about the
  // object tree of the mock catalog, which the loader walks when `schema_model` has no tables.
  vi.spyOn(ipc, 'schemaModel').mockResolvedValue({
    scope: { segments: [] },
    tables: [],
    relations: [],
    inferred: false,
    truncated: false,
  });
});

/** `|` marks the cursor. */
async function complete(source: string, explicit = false) {
  const session = await openSession();
  const pos = source.indexOf('|');
  const doc = source.replace('|', '');
  const fn = createCompletionSource(() => ({
    sessionId: session.sessionId,
    mode: 'sql',
    bracketQuotes: false,
    database: 'app_dev',
    schema: 'public',
  }));
  const state = EditorState.create({ doc });
  return fn(new CompletionContext(state, pos, explicit));
}

describe('createCompletionSource (mock catalog)', () => {
  it('suggests the columns of the table behind an alias: SELECT u.| FROM users u', async () => {
    const result = await complete('SELECT u.| FROM users u');
    const labels = (await Promise.resolve(result))!.options.map((o) => o.label);
    expect(labels).toEqual(expect.arrayContaining(['id', 'email']));
    // Only columns of users, not of other tables.
    expect(labels).not.toContain('categories');
    expect(result!.from).toBe('SELECT u.'.length);
  });

  it('completes tables after FROM and a word prefix', async () => {
    const result = await complete('SELECT * FROM us|');
    expect(result!.options.map((o) => o.label)).toContain('users');
  });

  it('stays quiet on an empty position unless asked', async () => {
    expect(await complete('SELECT * FROM users WHERE |')).toBeNull();
    const asked = await complete('SELECT * FROM users u WHERE |', true);
    expect(asked!.options.map((o) => o.label)).toContain('email');
  });

  it('does nothing without a session', async () => {
    const fn = createCompletionSource(() => ({
      sessionId: null,
      mode: 'sql',
      bracketQuotes: false,
      database: null,
      schema: null,
    }));
    const state = EditorState.create({ doc: 'SELECT u.' });
    expect(await fn(new CompletionContext(state, 9, true))).toBeNull();
  });
});

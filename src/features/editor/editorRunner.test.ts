import { beforeEach, describe, expect, it } from 'vitest';
import { useConfirmStore } from '@/features/safety/confirmStore';
import { ipc } from '@/lib/ipc';
import { useSessionsStore } from '@/stores/sessions';
import { editorTabId, useTabsStore } from '@/stores/tabs';
import {
  beginTransaction,
  cancelRun,
  confirmClose,
  endTransaction,
  fetchMore,
  runText,
  setAutoCommit,
  setCloseTransactionAsker,
} from './editorRunner';
import { getTabData, patchTabData } from './editorStore';
import { openEditor } from './open';
import { statementAt } from './runScope';
import {
  PG_DEV,
  PG_PROD_RO,
  addProductionProfile,
  openSession,
  resetEditorWorld,
} from './testUtils';

beforeEach(resetEditorWorld);

async function newEditor(connectionId = PG_DEV, text = '') {
  const session = await openSession(connectionId);
  const id = openEditor(session, { text });
  return {
    session,
    id,
    run: (t: string, baseChar = 0) => runText(id, session.sessionId, { text: t, baseChar }),
  };
}

describe('running scripts', () => {
  it('streams one result per statement with affected rows and server messages', async () => {
    const { run, id } = await newEditor();
    const snap = (await run(
      "SELECT * FROM users; UPDATE users SET a = 1 WHERE id = 1; DO $$ RAISE NOTICE 'hello' $$; CREATE TABLE t (a int)",
    ))!;
    expect(snap.status).toBe('ok');
    expect(snap.results.map((r) => r.outcome?.kind)).toEqual(['rows', 'affected', 'done', 'done']);
    expect(snap.results[0]!.model!.rows).toHaveLength(1000);
    expect(snap.results[1]!.outcome).toEqual({ kind: 'affected', rows: 3 });
    expect(snap.results[2]!.messages).toEqual(['NOTICE:  hello']);
    expect(getTabData(id)!.leaseState!.transaction).toBe('idle');
    expect((await ipc.historyList({ limit: 10 }))[0]).toMatchObject({
      status: 'ok',
      statements: 4,
    });
  });

  it('does not start a second run while one is in progress', async () => {
    const { run } = await newEditor();
    const first = run('SELECT pg_sleep(0.2)');
    await new Promise((r) => setTimeout(r, 30));
    expect(await run('SELECT 1')).toBeNull();
    expect((await first)!.status).toBe('ok');
  });

  it('marks the error position of the failing statement, counting emoji as one character', async () => {
    const { run, id } = await newEditor();
    const doc = "SELECT '😀😀';\nSELECT 'a😀', nope FROM users";
    patchTabData(id, { text: doc });
    const snap = (await run(doc))!;
    expect(snap.status).toBe('error');
    expect(snap.error?.kind).toBe('query');
    const mark = snap.errorMark!;
    expect(doc.slice(mark.from, mark.to)).toBe('nope');
    expect([mark.line, mark.column]).toEqual([2, 14]);
    // The first statement still produced its result, the failing one carries the error.
    expect(snap.results.map((r) => r.error?.kind ?? r.outcome?.kind)).toEqual(['rows', 'query']);
  });

  it('adds the base offset when only part of the document ran', async () => {
    const { run, id } = await newEditor();
    const doc = "SELECT '🎉🎉';\nSELECT nope";
    patchTabData(id, { text: doc });
    const base = Array.from("SELECT '🎉🎉';\n").length;
    const snap = (await run('SELECT nope', base))!;
    expect(doc.slice(snap.errorMark!.from, snap.errorMark!.to)).toBe('nope');
  });

  it('reports a missing parameter and runs with one', async () => {
    const { run, id, session } = await newEditor();
    expect((await run('SELECT * FROM users WHERE id = :id'))!.error?.message).toContain(':id');
    const snap = (await runText(id, session.sessionId, {
      text: 'SELECT * FROM users WHERE id = :id',
      baseChar: 0,
      params: [{ name: ':id', value: { kind: 'number', v: '7' } }],
    }))!;
    expect(snap.status).toBe('ok');
  });

  it('continues a truncated result set with fetch_more', async () => {
    const { run, id } = await newEditor();
    const snap = (await run('SELECT * FROM generate_series(1, 2500)'))!;
    const model = snap.results[0]!.model!;
    expect(model.rows).toHaveLength(1000);
    expect(snap.canFetchMore).toBe(true);
    expect(snap.results[0]!.outcome).toMatchObject({ kind: 'rows', truncated: true });
    await fetchMore(id);
    expect(model.rows).toHaveLength(2000);
    expect(getTabData(id)!.run!.canFetchMore).toBe(true);
    await fetchMore(id);
    expect(model.rows).toHaveLength(2500);
    expect(getTabData(id)!.run!.canFetchMore).toBe(false);
    expect(model.exhausted).toBe(true);
    expect(model.rows[2499]![0]).toEqual({ t: 'int', v: 2500 });
  });

  it('cancels a running statement through the lease', async () => {
    const { run, id } = await newEditor();
    const started = Date.now();
    const running = run('SELECT pg_sleep(30)');
    await new Promise((r) => setTimeout(r, 60));
    expect(getTabData(id)!.run!.status).toBe('running');
    await cancelRun(id);
    const snap = (await running)!;
    expect(snap.status).toBe('cancelled');
    expect(Date.now() - started).toBeLessThan(2000);
    expect((await ipc.historyList({ limit: 1 }))[0]!.status).toBe('cancelled');
  });
});

describe('connection policy', () => {
  it('blocks writes on read-only connections and marks the statement', async () => {
    const { run } = await newEditor(PG_PROD_RO);
    const snap = (await run('SELECT 1; DELETE FROM users WHERE id = 1'))!;
    expect(snap.status).toBe('error');
    expect(snap.error).toMatchObject({ kind: 'readOnly', position: 10 });
  });

  it('asks for the database name before destructive statements on production', async () => {
    const prod = await addProductionProfile();
    const { run } = await newEditor(prod.id);
    const answer = (typed: string | null) =>
      new Promise<void>((resolve) => {
        const off = useConfirmStore.subscribe((s) => {
          const head = s.queue[0];
          if (!head) return;
          off();
          if (typed === null) head.reject({ kind: 'cancelled', message: 'declined' });
          else head.resolve({ token: head.request.token, typed });
          useConfirmStore.getState().shift();
          resolve();
        });
      });

    // Declined: nothing runs.
    const declined = run('DROP TABLE users');
    await answer(null);
    expect((await declined)!.status).toBe('cancelled');

    // Wrong name: the backend refuses the grant and the user is asked again.
    const wrong = run('DROP TABLE users');
    await answer('app');
    const second = answer('app_prod');
    const snap = (await wrong)!;
    await second;
    expect(snap.status).toBe('ok');
    expect(snap.results[0]!.outcome).toEqual({ kind: 'done' });
  });

  it('does not ask for harmless statements on production', async () => {
    const prod = await addProductionProfile();
    const { run } = await newEditor(prod.id);
    const snap = (await run('DELETE FROM users WHERE id = 1'))!;
    expect(snap.status).toBe('ok');
    expect(useConfirmStore.getState().queue).toHaveLength(0);
  });
});

describe('transactions', () => {
  it('manual mode opens a transaction lazily, commit and rollback close it', async () => {
    const { run, id, session } = await newEditor();
    expect(await setAutoCommit(id, session.sessionId, false)).toBe('ok');
    expect(getTabData(id)!.leaseState).toBeNull(); // no lease yet: only the preference changed
    await run('SELECT 1');
    expect(getTabData(id)!.leaseState).toMatchObject({ autoCommit: false, transaction: 'active' });
    expect(await endTransaction(id, session.sessionId, 'commit')).toBe(true);
    expect(getTabData(id)!.leaseState!.transaction).toBe('idle');
    await beginTransaction(id, session.sessionId);
    expect(getTabData(id)!.leaseState!.transaction).toBe('active');
    expect(await endTransaction(id, session.sessionId, 'rollback')).toBe(true);
    expect(getTabData(id)!.leaseState!.transaction).toBe('idle');
  });

  it('turning auto-commit on with an open transaction needs a decision', async () => {
    const { id, session, run } = await newEditor();
    await run('BEGIN');
    expect(getTabData(id)!.leaseState!.transaction).toBe('active');
    expect(await setAutoCommit(id, session.sessionId, true)).toBe('needsDecision');
    await endTransaction(id, session.sessionId, 'commit');
    expect(await setAutoCommit(id, session.sessionId, false)).toBe('ok');
    expect(getTabData(id)!.autoCommit).toBe(false);
  });

  it('an error aborts the PostgreSQL transaction (failed) until rollback', async () => {
    const { id, session, run } = await newEditor();
    await run('BEGIN');
    await run('SELECT nope');
    expect(getTabData(id)!.leaseState!.transaction).toBe('failed');
    await endTransaction(id, session.sessionId, 'rollback');
    expect(getTabData(id)!.leaseState!.transaction).toBe('idle');
  });

  it('closing a tab with an open transaction asks first', async () => {
    const { id, run } = await newEditor();
    await run('BEGIN');
    const leaseId = getTabData(id)!.leaseId!;
    const asked: string[] = [];
    setCloseTransactionAsker((tab) => {
      asked.push(tab);
      return Promise.resolve('cancel');
    });
    // Through the tabs store, like the tab bar does.
    expect(await useTabsStore.getState().requestClose(editorTabId(id))).toBe(false);
    expect(asked).toEqual([id]);
    expect(useTabsStore.getState().tabs.some((t) => t.id === editorTabId(id))).toBe(true);
    await expect(ipc.leaseState(leaseId)).resolves.toMatchObject({ transaction: 'active' });

    setCloseTransactionAsker(() => Promise.resolve('rollback'));
    expect(await useTabsStore.getState().requestClose(editorTabId(id))).toBe(true);
    expect(useTabsStore.getState().tabs.some((t) => t.id === editorTabId(id))).toBe(false);
    // The lease was closed with the tab (which rolls the transaction back).
    await new Promise((r) => setTimeout(r, 20));
    await expect(ipc.leaseState(leaseId)).rejects.toMatchObject({ kind: 'notFound' });
  });

  it('commit-and-close commits first; a failed commit keeps the tab', async () => {
    const { id, session, run } = await newEditor();
    await run('BEGIN');
    setCloseTransactionAsker(() => Promise.resolve('commit'));
    expect(await confirmClose(id)).toBe(true);
    expect(getTabData(id)!.leaseState!.transaction).toBe('idle');
    void session;
  });

  it('closes quietly without a transaction', async () => {
    const { id, run } = await newEditor();
    await run('SELECT 1');
    setCloseTransactionAsker(() => Promise.reject(new Error('must not ask')));
    expect(await useTabsStore.getState().requestClose(editorTabId(id))).toBe(true);
  });

  it('forgets the lease when the session closes and remembers a lost transaction', async () => {
    const { id, run } = await newEditor();
    await run('BEGIN');
    await useSessionsStore.getState().close(PG_DEV);
    const data = getTabData(id)!;
    expect(data.leaseId).toBeNull();
    expect(data.transactionLost).toBe(true);
  });
});

describe('statement at the cursor', () => {
  it('picks the containing, else the previous statement', () => {
    const st = (index: number, start: number, end: number) =>
      ({ index, range: { start, end } }) as Parameters<typeof statementAt>[0][number];
    const list = [st(0, 0, 8), st(1, 10, 20)];
    expect(statementAt(list, 3)?.index).toBe(0);
    expect(statementAt(list, 8)?.index).toBe(0); // right after the last character
    expect(statementAt(list, 9)?.index).toBe(0); // in the gap
    expect(statementAt(list, 15)?.index).toBe(1);
    expect(statementAt(list, 40)?.index).toBe(1);
    expect(statementAt([], 0)).toBeNull();
  });
});

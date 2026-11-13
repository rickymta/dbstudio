import { describe, expect, it } from 'vitest';
import { ipc } from '@/lib/ipc';
import type { ExplainRequest } from '@/lib/types';

const PG = '00000000-0000-4000-8000-0000000000b1';
const MYSQL = '00000000-0000-4000-8000-0000000000b2';

const request = (analyze: boolean, text = 'select 1'): ExplainRequest => ({
  queryId: 'q',
  text,
  analyze,
});

describe('mock plans', () => {
  it('answers with the captured PostgreSQL plans', async () => {
    const { sessionId } = await ipc.sessionOpen(PG);
    const plain = await ipc.planExplain(sessionId, null, request(false, 'select * from t'));
    expect(plain.raw.format).toBe('postgresJson');
    expect(plain.raw.statement).toBe('select * from t');
    expect(plain.raw.analyzed).toBe(false);
    expect(plain.tree?.analyzed).toBe(false);
    const analyzed = await ipc.planExplain(sessionId, null, request(true));
    expect(analyzed.raw.analyzed && analyzed.raw.rolledBack).toBe(true);
    expect(analyzed.tree?.analyzed).toBe(true);
    expect(analyzed.tree?.hotPath.length).toBeGreaterThan(1);
    // The tree belongs to the raw text: parsing the raw plan returns it.
    expect(await ipc.planParse(analyzed.raw)).toEqual(analyzed.tree);
  });

  it('knows MySQL plans and rejects empty statements and unknown plans', async () => {
    const { sessionId } = await ipc.sessionOpen(MYSQL, 'secret');
    const tree = await ipc.planExplain(sessionId, null, request(true));
    expect(tree.raw.format).toBe('mySqlTree');
    await expect(ipc.planExplain(sessionId, null, request(false, '  '))).rejects.toMatchObject({
      kind: 'query',
    });
    await expect(ipc.planParse({ ...tree.raw, text: 'nope' })).rejects.toMatchObject({
      kind: 'other',
    });
  });

  it('round-trips a saved plan through the file dialogs', async () => {
    const { sessionId } = await ipc.sessionOpen(PG);
    const { raw } = await ipc.planExplain(sessionId, null, request(false));
    expect(await ipc.planFileSave(raw)).toMatch(/\.dbplan\.json$/);
    expect(await ipc.planFileOpen()).toEqual(raw);
  });
});

// End-to-end with the real ipc wrapper over the mock backend in "late" mode: row batches and
// statementEnd arrive AFTER the fetch promise resolved, like large Tauri Channel payloads.
import { afterEach, beforeAll, describe, expect, it } from 'vitest';
import { mock } from '@/lib/mock';
import { GridModel } from './gridModel';
import { openMockSession, pgPath } from './testUtils';

let sessionId: string;
beforeAll(async () => {
  sessionId = await openMockSession();
});
afterEach(() => mock.setFetchMode('inline'));

function waitFor(cond: () => boolean, ms = 8000): Promise<void> {
  return new Promise((resolve, reject) => {
    const t0 = Date.now();
    const tick = () => {
      if (cond()) return resolve();
      if (Date.now() - t0 > ms) return reject(new Error('timeout'));
      setTimeout(tick, 10);
    };
    tick();
  });
}

describe('GridModel with late-arriving batches', () => {
  it('counts late batches into the page, does not end early, does not duplicate on loadMore', async () => {
    mock.setFetchMode('late');
    const m = new GridModel({ sessionId, table: pgPath('events'), pageSize: 1000 });
    m.ensureLoaded();
    await waitFor(() => m.status === 'ready');
    expect(m.rows).toHaveLength(1000); // all rows were there when the page completed
    expect(m.exhausted).toBe(false); // not mistaken for the end of the data

    m.loadMore();
    await waitFor(() => m.status === 'ready' && m.rows.length === 2000);
    const ids = m.rows.map((r) => (r[0] as { v: number }).v);
    expect(ids).toEqual(Array.from({ length: 2000 }, (_, i) => i + 1)); // no gaps, no duplicates
    m.dispose();
  });

  it('grows the page size and ends on a short page', async () => {
    const m = new GridModel({ sessionId, table: pgPath('categories') }); // 24 rows
    m.ensureLoaded();
    await waitFor(() => m.status === 'ready');
    expect(m.rows).toHaveLength(24);
    expect(m.exhausted).toBe(true);
    m.dispose();
  });
});

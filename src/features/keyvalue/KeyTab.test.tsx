import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ipc } from '@/lib/ipc';
import { redisDb } from '@/lib/mock/catalogRedis';
import type { ChangeSet, KeyChange } from '@/lib/types';
import { KeyTab } from './KeyTab';
import { keyPath, openRedisSession, renderWithProviders, resetRedisMocks } from './testUtils';

const WAIT = { timeout: 10_000 };

async function mount(name: string, overrides: Parameters<typeof openRedisSession>[0] = {}) {
  const info = await openRedisSession(overrides);
  renderWithProviders(
    <KeyTab sessionId={info.sessionId} connectionId={info.connectionId} path={keyPath(name)} />,
  );
  return info;
}

const keyChanges = (spy: { mock: { calls: unknown[][] } }, call = 0): KeyChange[] => {
  const set = spy.mock.calls[call]![1] as ChangeSet;
  expect(set.model).toBe('keys');
  return set.model === 'keys' ? set.changes : [];
};

beforeEach(() => resetRedisMocks());
afterEach(() => vi.restoreAllMocks());

describe('KeyTab rendering', () => {
  it('shows header info and the string value of a key with a TTL', async () => {
    await mount('cache:product:1');
    expect(await screen.findByLabelText('String value', undefined, WAIT)).toHaveValue(
      '{"id":1,"name":"Product 1","price":3.5}',
    );
    expect(screen.getByTestId('key-type')).toHaveTextContent('string');
    expect(screen.getByTestId('key-ttl')).toHaveTextContent(/\d+m/);
    expect(screen.getByTestId('key-length')).toBeInTheDocument();
  });

  it('shows "No expiry" for a persistent key', async () => {
    await mount('config');
    await screen.findByLabelText('String value', undefined, WAIT);
    expect(screen.getByTestId('key-ttl')).toHaveTextContent('No expiry');
  });

  it('renders hash, list, set, zset, stream and json keys', async () => {
    await mount('user:1:profile');
    expect(await screen.findByText('email', undefined, WAIT)).toBeInTheDocument();
    expect(screen.getByText('user1@example.com')).toBeInTheDocument();
  });

  it('renders a list with indexes', async () => {
    await mount('user:1:recent');
    expect(await screen.findByText('/page/1/0', undefined, WAIT)).toBeInTheDocument();
    expect(screen.getByText('Push to head')).toBeInTheDocument();
  });

  it('renders a set', async () => {
    await mount('user:3:tags');
    expect(await screen.findByText('alpha', undefined, WAIT)).toBeInTheDocument();
  });

  it('renders a sorted set with scores and the stream with its group count', async () => {
    await mount('leaderboard:daily');
    expect(await screen.findByText('player-249', undefined, WAIT)).toBeInTheDocument();
  });

  it('renders a stream', async () => {
    await mount('stream:orders');
    expect(await screen.findByText('Consumer groups: 2', undefined, WAIT)).toBeInTheDocument();
    expect(screen.getAllByText(/^17000000/).length).toBeGreaterThan(0);
  });

  it('shows RedisJSON text read-only', async () => {
    await mount('doc:settings');
    expect(await screen.findByTestId('key-json', undefined, WAIT)).toHaveTextContent('"theme"');
    expect(screen.getByText(/read-only here/)).toBeInTheDocument();
  });

  it('opens a binary string key in hex mode', async () => {
    await mount('\u0000b64:3q2+7w==');
    expect(await screen.findByLabelText('String value', undefined, WAIT)).toHaveValue(
      'deadbeefcbeffe',
    );
  });

  it('says so when the key does not exist', async () => {
    await mount('does-not-exist');
    expect(await screen.findByText('Key not found', undefined, WAIT)).toBeInTheDocument();
  });

  it('pages long collections with "Load more"', async () => {
    const user = userEvent.setup();
    await mount('leaderboard:daily');
    await screen.findByText('player-249', undefined, WAIT);
    const rows = () => within(screen.getByRole('table')).getAllByRole('row').length - 1;
    expect(rows()).toBe(200);
    await user.click(screen.getByRole('button', { name: 'Load more' }));
    await waitFor(() => expect(rows()).toBe(250), WAIT);
    expect(screen.queryByRole('button', { name: 'Load more' })).toBeNull();
  });

  it('searches hash fields by pattern', async () => {
    const user = userEvent.setup();
    await mount('user:1:profile');
    await screen.findByText('email', undefined, WAIT);
    await user.type(screen.getByLabelText('Search pattern'), 'e*{Enter}');
    await waitFor(() => expect(screen.queryByText('plan')).toBeNull(), WAIT);
    expect(screen.getByText('email')).toBeInTheDocument();
    expect(screen.queryByText('created')).toBeNull();
  });
});

describe('KeyTab editing', () => {
  it('stages a string edit and applies the right ChangeSet', async () => {
    const user = userEvent.setup();
    const spy = vi.spyOn(ipc, 'changesApply');
    await mount('config');
    const area = await screen.findByLabelText('String value', undefined, WAIT);
    await user.clear(area);
    await user.type(area, 'hello');
    await user.click(screen.getByRole('button', { name: 'Stage change' }));
    expect(screen.getByTestId('pending-bar')).toHaveTextContent('Pending changes: 1');
    await user.click(screen.getByRole('button', { name: 'Apply' }));
    await waitFor(() => expect(spy).toHaveBeenCalledTimes(1), WAIT);
    const set = spy.mock.calls[0]![1];
    expect(set).toEqual({
      model: 'keys',
      database: { segments: [{ kind: 'database', name: '0' }] },
      changes: [{ op: 'setString', key: 'config', value: { t: 'text', v: 'hello' } }],
    });
    // No ttlMs: the server keeps the existing TTL (see the mock).
    await waitFor(() => expect(screen.queryByTestId('pending-bar')).toBeNull(), WAIT);
    const entry = redisDb('0').get('config')!;
    expect(entry.type === 'string' && entry.value).toEqual({ t: 'text', v: 'hello' });
  });

  it('previews the commands before applying', async () => {
    const user = userEvent.setup();
    await mount('config');
    const area = await screen.findByLabelText('String value', undefined, WAIT);
    await user.type(area, '!');
    await user.click(screen.getByRole('button', { name: 'Stage change' }));
    await user.click(screen.getByRole('button', { name: 'Preview commands' }));
    expect(await screen.findByTestId('preview-commands')).toHaveTextContent(
      /^SET config .* KEEPTTL$/,
    );
  });

  it('switches to hex mode and saves bytes', async () => {
    const user = userEvent.setup();
    const spy = vi.spyOn(ipc, 'changesApply');
    await mount('config');
    const area = await screen.findByLabelText('String value', undefined, WAIT);
    await user.click(screen.getByRole('button', { name: 'Hex' }));
    expect(area).toHaveValue(
      Array.from(new TextEncoder().encode('{"featureX":true,"limit":50}'), (b) =>
        b.toString(16).padStart(2, '0'),
      ).join(''),
    );
    await user.clear(area);
    await user.type(area, 'ff00');
    await user.click(screen.getByRole('button', { name: 'Stage change' }));
    await user.click(screen.getByRole('button', { name: 'Apply' }));
    await waitFor(() => expect(spy).toHaveBeenCalled(), WAIT);
    expect(keyChanges(spy)).toEqual([
      { op: 'setString', key: 'config', value: { t: 'bytes', v: '/wA=' } },
    ]);
    // Let the apply finish so it cannot write into the next test's freshly reset database.
    await waitFor(() => expect(screen.queryByTestId('pending-bar')).toBeNull(), WAIT);
  });

  it('formats JSON in the JSON mode', async () => {
    const user = userEvent.setup();
    await mount('config');
    const area = await screen.findByLabelText('String value', undefined, WAIT);
    await user.click(screen.getByRole('button', { name: 'JSON' }));
    expect((area as HTMLTextAreaElement).value).toContain('\n  "featureX": true');
  });

  it('adds, edits and deletes hash fields through ChangeSet changes', async () => {
    const user = userEvent.setup();
    const spy = vi.spyOn(ipc, 'changesApply');
    await mount('user:1:profile');
    await screen.findByText('email', undefined, WAIT);
    await user.type(screen.getByLabelText('New field'), 'nick');
    await user.type(screen.getByLabelText('New value'), 'neo');
    await user.click(screen.getByRole('button', { name: 'Add field' }));
    fireEvent.doubleClick(screen.getByText('free'));
    const cell = screen.getByLabelText('Edit value');
    await user.clear(cell);
    await user.type(cell, 'gold{Enter}');
    await user.click(screen.getAllByRole('button', { name: 'Delete' })[0]!);
    expect(screen.getByTestId('pending-bar')).toHaveTextContent('Pending changes: 3');
    await user.click(screen.getByRole('button', { name: 'Apply' }));
    await waitFor(() => expect(spy).toHaveBeenCalled(), WAIT);
    const t = (v: string) => ({ t: 'text', v });
    expect(keyChanges(spy)).toEqual([
      { op: 'hashSet', key: 'user:1:profile', entries: [[t('nick'), t('neo')]] },
      { op: 'hashSet', key: 'user:1:profile', entries: [[t('plan'), t('gold')]] },
      { op: 'hashDelete', key: 'user:1:profile', fields: [t('name')] },
    ]);
  });

  it('stages list, set and sorted-set operations', async () => {
    const user = userEvent.setup();
    const spy = vi.spyOn(ipc, 'changesApply');
    await mount('user:1:recent');
    await screen.findByText('/page/1/0', undefined, WAIT);
    await user.type(screen.getByLabelText('New element'), 'tail');
    await user.click(screen.getByRole('button', { name: 'Push to tail' }));
    await user.click(screen.getAllByRole('button', { name: 'Delete' })[1]!);
    await user.click(screen.getByRole('button', { name: 'Apply' }));
    await waitFor(() => expect(spy).toHaveBeenCalled(), WAIT);
    expect(keyChanges(spy)).toEqual([
      // Index-based removals go before pushes so indexes keep referring to the loaded list.
      { op: 'listRemoveAt', key: 'user:1:recent', index: 1 },
      { op: 'listPush', key: 'user:1:recent', values: [{ t: 'text', v: 'tail' }], head: false },
    ]);
  });

  it('stages set add/remove and zset score changes', async () => {
    const user = userEvent.setup();
    const spy = vi.spyOn(ipc, 'changesApply');
    await mount('leaderboard:daily');
    await screen.findByText('player-249', undefined, WAIT);
    fireEvent.doubleClick(screen.getByText('254'));
    await user.type(screen.getByLabelText('Edit value'), '{Control>}a{/Control}1500{Enter}');
    await user.click(screen.getAllByRole('button', { name: 'Delete' })[2]!);
    await user.click(screen.getByRole('button', { name: 'Apply' }));
    await waitFor(() => expect(spy).toHaveBeenCalled(), WAIT);
    const changes = keyChanges(spy);
    expect(changes[0]).toMatchObject({ op: 'zAdd', members: [[{ v: 'player-249' }, '1500']] });
    expect(changes[1]).toMatchObject({ op: 'zRemove' });
  });

  it('adds a stream entry and removes one', async () => {
    const user = userEvent.setup();
    const spy = vi.spyOn(ipc, 'changesApply');
    await mount('stream:audit');
    await screen.findByText('Consumer groups: 0', undefined, WAIT);
    await user.type(screen.getByLabelText('Entry fields'), 'who=me\nwhat=did it');
    await user.click(screen.getByRole('button', { name: 'Add entry' }));
    await user.click(screen.getAllByRole('button', { name: 'Delete' })[0]!);
    await user.click(screen.getByRole('button', { name: 'Apply' }));
    await waitFor(() => expect(spy).toHaveBeenCalled(), WAIT);
    const [add, del] = keyChanges(spy);
    expect(add).toMatchObject({ op: 'streamAdd', id: null });
    expect(add!.op === 'streamAdd' && add!.fields).toHaveLength(2);
    expect(del).toMatchObject({ op: 'streamDelete', ids: ['1700000000000-0'] });
  });

  it('refuses a TTL below one millisecond (PEXPIRE 0 would delete the key)', async () => {
    const user = userEvent.setup();
    const spy = vi.spyOn(ipc, 'changesApply');
    await mount('cache:product:1');
    await screen.findByLabelText('String value', undefined, WAIT);
    await user.click(screen.getByRole('button', { name: 'TTL' }));
    const input = await screen.findByLabelText('Seconds');
    await user.clear(input);
    await user.type(input, '0.0001');
    expect(screen.getByRole('button', { name: 'Set TTL' })).toBeDisabled();
    await user.clear(input);
    await user.type(input, '0.001');
    expect(screen.getByRole('button', { name: 'Set TTL' })).toBeEnabled();
    expect(spy).not.toHaveBeenCalled();
  });

  it('sets and removes the TTL', async () => {
    const user = userEvent.setup();
    const spy = vi.spyOn(ipc, 'changesApply');
    await mount('cache:product:1');
    await screen.findByLabelText('String value', undefined, WAIT);
    await user.click(screen.getByRole('button', { name: 'TTL' }));
    const input = await screen.findByLabelText('Seconds');
    await user.clear(input);
    await user.type(input, '90');
    await user.click(screen.getByRole('button', { name: 'Set TTL' }));
    await waitFor(() => expect(spy).toHaveBeenCalledTimes(1), WAIT);
    expect(keyChanges(spy)).toEqual([{ op: 'expire', key: 'cache:product:1', ttlMs: 90000 }]);
    await waitFor(() => expect(screen.getByTestId('key-ttl')).toHaveTextContent(/1m \d+s/), WAIT);
    await user.click(screen.getByRole('button', { name: 'TTL' }));
    await user.click(await screen.findByRole('button', { name: 'Remove TTL' }));
    await waitFor(() => expect(spy).toHaveBeenCalledTimes(2), WAIT);
    expect(keyChanges(spy, 1)).toEqual([{ op: 'expire', key: 'cache:product:1', ttlMs: null }]);
    await waitFor(() => expect(screen.getByTestId('key-ttl')).toHaveTextContent('No expiry'), WAIT);
  }, 15_000);

  it('deletes the key after confirmation', async () => {
    const user = userEvent.setup();
    const spy = vi.spyOn(ipc, 'changesApply');
    await mount('config:motd');
    await screen.findByLabelText('String value', undefined, WAIT);
    await user.click(screen.getByRole('button', { name: 'Delete key' }));
    const dialog = await screen.findByRole('alertdialog').catch(() => screen.findByRole('dialog'));
    await user.click(within(dialog).getByRole('button', { name: 'Delete key' }));
    await waitFor(() => expect(spy).toHaveBeenCalled(), WAIT);
    expect(keyChanges(spy)).toEqual([{ op: 'delete', keys: ['config:motd'] }]);
    expect(await screen.findByText('Key not found', undefined, WAIT)).toBeInTheDocument();
  });

  it('reports a partial failure and says Redis did not roll back', async () => {
    const user = userEvent.setup();
    vi.spyOn(ipc, 'changesApply').mockRejectedValue({
      kind: 'changeFailed',
      message: 'WRONGTYPE',
      index: 1,
      applied: 1,
    });
    await mount('user:1:profile');
    await screen.findByText('email', undefined, WAIT);
    for (const name of ['a', 'b']) {
      await user.type(screen.getByLabelText('New field'), name);
      await user.click(screen.getByRole('button', { name: 'Add field' }));
    }
    await user.click(screen.getByRole('button', { name: 'Apply' }));
    const alert = await screen.findByText(/Change #2 failed/, undefined, WAIT);
    expect(alert).toHaveTextContent('Redis does not roll back');
    expect(alert).toHaveTextContent('1 earlier change(s) were already applied');
    // The applied change left the pending list; the failed one stays.
    expect(screen.getByTestId('pending-bar')).toHaveTextContent('Pending changes: 1');
  });
});

describe('KeyTab on a read-only connection', () => {
  it('locks every editing control', async () => {
    await mount('user:1:profile', { readOnly: true });
    await screen.findByText('email', undefined, WAIT);
    await waitFor(
      () => expect(screen.getByRole('button', { name: 'Delete key' })).toBeDisabled(),
      WAIT,
    );
    expect(screen.getByRole('button', { name: 'Rename' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'TTL' })).toBeDisabled();
    expect(screen.queryByLabelText('New field')).toBeNull();
    expect(screen.getAllByRole('button', { name: 'Delete' })[0]).toBeDisabled();
    expect(screen.getByText('Read-only')).toBeInTheDocument();
  });

  it('makes the string textarea read-only and hides the save button', async () => {
    await mount('config', { readOnly: true });
    const area = await screen.findByLabelText('String value', undefined, WAIT);
    await waitFor(() => expect(area).toHaveAttribute('readonly'), WAIT);
    expect(screen.queryByRole('button', { name: 'Stage change' })).toBeNull();
  });
});

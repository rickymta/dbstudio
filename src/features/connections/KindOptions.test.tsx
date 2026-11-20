import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState } from 'react';
import { beforeEach, describe, expect, it } from 'vitest';
import i18n from '@/lib/i18n';
import type { DbKind } from '@/lib/types';
import { KindOptions } from './KindOptions';
import {
  kindOptionsFor,
  optionsForKind,
  readKindOption,
  writeKindOption,
} from './kindOptionFields';

beforeEach(async () => {
  await i18n.changeLanguage('en');
});

function Harness({ kind, initial = {} }: { kind: DbKind; initial?: Record<string, string> }) {
  const [options, setOptions] = useState(initial);
  return (
    <>
      <KindOptions kind={kind} value={options} onChange={setOptions} />
      <output data-testid="options">{JSON.stringify(options)}</output>
    </>
  );
}

const stored = () => JSON.parse(screen.getByTestId('options').textContent ?? '{}') as unknown;

describe('kindOptions model', () => {
  it('has no fields for kinds without driver options', () => {
    expect(kindOptionsFor('postgres')).toEqual([]);
    expect(kindOptionsFor('scylladb')).toBe(kindOptionsFor('cassandra'));
  });

  it('reads defaults and stores only values that differ from them', () => {
    const [localDc, consistency, lwt] = kindOptionsFor('cassandra');
    expect(readKindOption({}, consistency!)).toBe('LOCAL_ONE');
    expect(readKindOption({ 'cql.consistency': 'quorum' }, consistency!)).toBe('QUORUM');
    expect(readKindOption({}, lwt!)).toBe(true);
    expect(readKindOption({ 'cql.lwtEdits': 'false' }, lwt!)).toBe(false);
    expect(writeKindOption({ a: '1' }, lwt!, false)).toEqual({ a: '1', 'cql.lwtEdits': 'false' });
    expect(writeKindOption({ 'cql.lwtEdits': 'false' }, lwt!, true)).toEqual({});
    expect(writeKindOption({ 'cql.localDc': 'dc1' }, localDc!, '  ')).toEqual({});
  });

  it('drops the options of another driver when the kind changes', () => {
    const options = { 'cql.localDc': 'dc1', authSource: 'admin', 'clickhouse.secure': 'true' };
    expect(optionsForKind(options, 'postgres')).toEqual({ authSource: 'admin' });
    expect(optionsForKind(options, 'scylladb')).toEqual({
      'cql.localDc': 'dc1',
      authSource: 'admin',
    });
    const same = { authSource: 'admin' };
    expect(optionsForKind(same, 'mysql')).toBe(same);
  });
});

describe('KindOptions', () => {
  it('renders nothing for PostgreSQL', () => {
    render(<Harness kind="postgres" />);
    expect(screen.queryByTestId('kind-options')).toBeNull();
  });

  it('edits the Cassandra options', async () => {
    const user = userEvent.setup();
    render(<Harness kind="cassandra" />);
    await user.type(screen.getByLabelText('Local datacenter'), 'dc1');
    await user.click(screen.getByRole('combobox', { name: 'Consistency level' }));
    await user.click(await screen.findByRole('option', { name: 'LOCAL_QUORUM' }));
    await user.click(
      screen.getByRole('switch', { name: 'Edit rows with lightweight transactions' }),
    );
    expect(stored()).toEqual({
      'cql.localDc': 'dc1',
      'cql.consistency': 'LOCAL_QUORUM',
      'cql.lwtEdits': 'false',
    });
  });

  it('edits the ClickHouse options and shows the stored ones', async () => {
    const user = userEvent.setup();
    render(<Harness kind="clickhouse" initial={{ 'clickhouse.compression': 'none' }} />);
    expect(screen.getByRole('combobox', { name: 'Compression' })).toHaveTextContent('None');
    await user.click(screen.getByRole('combobox', { name: 'Data editing' }));
    await user.click(await screen.findByRole('option', { name: 'Insert only' }));
    expect(stored()).toEqual({
      'clickhouse.compression': 'none',
      'clickhouse.editing': 'insertOnly',
    });
  });
});

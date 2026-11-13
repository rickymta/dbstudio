import { act, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { FilterBar } from './FilterBar';
import { GridModel } from './gridModel';
import type { FilterRowDraft } from './request';
import { resetDataStores, useDataUiStore } from './store';
import { renderWithProviders } from './testUtils';

const TAB = 'table:test:filter';
const row = (p: Partial<FilterRowDraft>): FilterRowDraft => ({
  id: 'r1',
  enabled: true,
  column: 'status',
  op: 'eq',
  value: '',
  ...p,
});

let model: GridModel;

beforeEach(() => {
  model = new GridModel({ sessionId: 's', table: { segments: [] } });
  model.columns = [
    { name: 'id', typeName: 'int', valueKind: 'int' },
    { name: 'status', typeName: 'text', valueKind: 'text' },
  ];
  model.applyFilter = vi.fn();
  model.clearFilter = vi.fn();
});

afterEach(() => resetDataStores());

const draftOf = () => useDataUiStore.getState().byTab[TAB]!.draft;

describe('FilterBar', () => {
  it('adds and removes conditions', async () => {
    const user = userEvent.setup();
    renderWithProviders(<FilterBar tabId={TAB} model={model} />);
    expect(screen.queryByLabelText('Value')).toBeNull();
    await user.click(screen.getByRole('button', { name: /add condition/i }));
    await user.click(screen.getByRole('button', { name: /add condition/i }));
    expect(screen.getAllByLabelText('Value')).toHaveLength(2);
    expect(draftOf().rows[0]).toMatchObject({ column: 'id', op: 'eq', enabled: true });
    await user.click(screen.getAllByRole('button', { name: 'Remove condition' })[0]!);
    expect(screen.getAllByLabelText('Value')).toHaveLength(1);
  });

  it('applies typed values with the Apply button and Enter', async () => {
    const user = userEvent.setup();
    renderWithProviders(<FilterBar tabId={TAB} model={model} />);
    await user.click(screen.getByRole('button', { name: /add condition/i }));
    await user.type(screen.getByLabelText('Value'), 'paid');
    await user.click(screen.getByRole('button', { name: 'Apply' }));
    expect(model.applyFilter).toHaveBeenCalledTimes(1);
    expect(vi.mocked(model.applyFilter).mock.calls[0]![0].rows[0]).toMatchObject({ value: 'paid' });
    await user.type(screen.getByLabelText('Value'), '{Enter}');
    expect(model.applyFilter).toHaveBeenCalledTimes(2);
  });

  it('shows the comma-separated hint for `in` and no value box for isNull', () => {
    act(() => {
      useDataUiStore.getState().patch(TAB, {
        draft: {
          mode: 'builder',
          raw: '',
          rows: [row({ id: 'a', op: 'in', value: '1, 2' }), row({ id: 'b', op: 'isNull' })],
        },
      });
    });
    renderWithProviders(<FilterBar tabId={TAB} model={model} />);
    const values = screen.getAllByLabelText('Value');
    expect(values).toHaveLength(1); // the isNull row has no value input
    expect(values[0]).toHaveValue('1, 2');
    expect(values[0]).toHaveAttribute('placeholder', 'a, b, c');
  });

  it('toggles a condition on and off', async () => {
    const user = userEvent.setup();
    act(() => {
      useDataUiStore
        .getState()
        .patch(TAB, { draft: { mode: 'builder', raw: '', rows: [row({})] } });
    });
    renderWithProviders(<FilterBar tabId={TAB} model={model} />);
    await user.click(screen.getByRole('checkbox', { name: 'Enable condition' }));
    expect(draftOf().rows[0]!.enabled).toBe(false);
  });

  it('raw WHERE mode applies the typed clause; Clear resets the draft and the model', async () => {
    const user = userEvent.setup();
    renderWithProviders(<FilterBar tabId={TAB} model={model} />);
    await user.click(screen.getByRole('button', { name: 'Raw WHERE' }));
    await user.type(screen.getByLabelText('WHERE clause'), "status = 'paid'{Enter}");
    expect(vi.mocked(model.applyFilter).mock.calls[0]![0]).toMatchObject({
      mode: 'raw',
      raw: "status = 'paid'",
    });
    await user.click(screen.getByRole('button', { name: 'Clear filter' }));
    expect(model.clearFilter).toHaveBeenCalled();
    expect(draftOf().raw).toBe('');
  });

  it('shows the server error under the bar while a filter is active', () => {
    model.applied = { filters: [], rawWhere: 'bad !!' };
    model.status = 'error';
    model.error = { kind: 'query', message: 'syntax error at or near "!!"' };
    renderWithProviders(<FilterBar tabId={TAB} model={model} />);
    expect(screen.getByRole('alert')).toHaveTextContent('syntax error at or near "!!"');
  });
});

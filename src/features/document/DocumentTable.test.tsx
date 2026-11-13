import { cleanup, render, screen } from '@testing-library/react';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { polyfillLayoutApis } from '@/features/data/testUtils';
import type { GridModel } from '@/features/data/gridModel';
import type { Value } from '@/lib/types';
import { DocumentTable } from './DocumentTable';

beforeAll(() => polyfillLayoutApis());
afterEach(cleanup);

const docs = (n: number): Value[][] =>
  Array.from({ length: n }, (_, i) => [
    {
      t: 'document',
      v: [
        ['_id', { t: 'int', v: i }],
        ['name', { t: 'text', v: `user-${i}` }],
      ],
    } as Value,
  ]);

describe('DocumentTable', () => {
  it('draws only a window of a large result and asks for the next page near the end', () => {
    const loadMore = vi.fn();
    const model = {
      rows: docs(5000),
      sort: [],
      toggleSort: vi.fn(),
      loadMore,
    } as unknown as GridModel;
    render(
      <DocumentTable
        model={model}
        columns={[
          { name: '_id', sortable: true },
          { name: 'name', sortable: true },
        ]}
        selectedId={null}
        timestamp="utc"
        onSelect={vi.fn()}
        onOpen={vi.fn()}
      />,
    );
    const drawn = screen.getAllByRole('gridcell').length / 2;
    expect(drawn).toBeGreaterThan(0);
    expect(drawn).toBeLessThan(100);
    expect(screen.getByText('user-0')).toBeInTheDocument();
    expect(screen.queryByText('user-4999')).not.toBeInTheDocument();
    // Far from the end of 5000 rows: nothing is requested yet.
    expect(loadMore).not.toHaveBeenCalled();
  });

  it('requests the next page when the loaded rows end inside the window', () => {
    const loadMore = vi.fn();
    const model = {
      rows: docs(10),
      sort: [],
      toggleSort: vi.fn(),
      loadMore,
    } as unknown as GridModel;
    render(
      <DocumentTable
        model={model}
        columns={[{ name: 'name', sortable: true }]}
        selectedId={null}
        timestamp="utc"
        onSelect={vi.fn()}
        onOpen={vi.fn()}
      />,
    );
    expect(loadMore).toHaveBeenCalled();
  });
});

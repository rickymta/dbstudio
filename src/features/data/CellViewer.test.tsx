import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { useToastStore } from '@/components/ui/toastStore';
import { ipc } from '@/lib/ipc';
import type { Value } from '@/lib/types';
import { CellViewer, ValueView } from './CellViewer';
import { GridModel } from './gridModel';
import { collectExpandPaths } from './treeModel';
import { resetDataStores, useDataUiStore } from './store';
import { renderWithProviders } from './testUtils';

afterEach(() => resetDataStores());

const b64 = (bytes: number[]) => btoa(String.fromCharCode(...bytes));

describe('ValueView', () => {
  it('renders JSON as a collapsible tree and as pretty text', async () => {
    const user = userEvent.setup();
    const value: Value = {
      t: 'json',
      v: JSON.stringify({ n: 7, tags: ['a', 'b'], nested: { deep: { x: 1 } } }),
    };
    renderWithProviders(<ValueView value={value} />);
    expect(screen.getByText('tags')).toBeInTheDocument();
    expect(screen.getByText('"a"')).toBeInTheDocument();

    // Collapse the tags node.
    const tags = screen.getByText('tags').closest('button')!;
    expect(tags).toHaveAttribute('aria-expanded', 'true');
    await user.click(tags);
    expect(screen.queryByText('"a"')).toBeNull();

    // Deep nodes (depth >= 2) start collapsed; Expand all opens them.
    expect(screen.queryByText('x')).toBeNull();
    await user.click(screen.getByRole('button', { name: 'Expand all' }));
    expect(screen.getByText('x')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Text' }));
    expect(document.querySelector('pre')!.textContent).toBe(
      JSON.stringify({ n: 7, tags: ['a', 'b'], nested: { deep: { x: 1 } } }, null, 2),
    );
  });

  it('renders bytes as a hex dump with size and no preview for non-images', () => {
    const bytes = Array.from(new TextEncoder().encode('Hello, bytes!'));
    renderWithProviders(<ValueView value={{ t: 'bytes', v: b64(bytes) }} />);
    expect(screen.getByText(/13 B \(13 bytes\)/)).toBeInTheDocument();
    const dump = document.querySelector('pre')!.textContent!;
    expect(dump).toContain('00000000  48 65 6c 6c 6f 2c 20 62  79 74 65 73 21');
    expect(dump).toContain('|Hello, bytes!|');
    expect(screen.queryByRole('img')).toBeNull();
  });

  it('previews images recognized by magic bytes', () => {
    const png = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0];
    renderWithProviders(<ValueView value={{ t: 'bytes', v: b64(png) }} />);
    const img = screen.getByRole('img');
    expect(img.getAttribute('src')).toBe(`data:image/png;base64,${b64(png)}`);
    expect(screen.getByText('image/png')).toBeInTheDocument();
  });

  it('gives NULL its own indicator, distinct from the empty string', () => {
    const { unmount } = renderWithProviders(<ValueView value={{ t: 'null' }} />);
    expect(screen.getByText('NULL')).toBeInTheDocument();
    expect(screen.getByText(/not an empty string/)).toBeInTheDocument();
    unmount();
    renderWithProviders(<ValueView value={{ t: 'text', v: '' }} />);
    expect(screen.queryByText('NULL')).toBeNull();
    expect(screen.getByText('Empty string')).toBeInTheDocument();
  });

  it('shows long text with a character count', () => {
    renderWithProviders(<ValueView value={{ t: 'text', v: `line1\nline2 ${'x'.repeat(100)}` }} />);
    expect(screen.getByText('112 characters')).toBeInTheDocument();
  });
});

describe('ValueView lossless json', () => {
  const text = '{"id": 9007199254740993, "x": 1e400, "f": 0.1000000000000000055}';

  it('shows numbers verbatim in the tree and in text mode', async () => {
    const user = userEvent.setup();
    renderWithProviders(<ValueView value={{ t: 'json', v: text }} />);
    for (const n of ['9007199254740993', '1e400', '0.1000000000000000055']) {
      expect(screen.getByText(n)).toBeInTheDocument();
    }
    expect(screen.queryByText('"9007199254740993"')).toBeNull(); // not a quoted string
    await user.click(screen.getByRole('button', { name: 'Text' }));
    const pre = document.querySelector('pre')!.textContent!;
    expect(pre).toContain('"id": 9007199254740993');
    expect(pre).toContain('"x": 1e400');
    expect(pre).toContain('"f": 0.1000000000000000055');
  });

  it('falls back to plain text for invalid json', () => {
    renderWithProviders(<ValueView value={{ t: 'json', v: '{oops: not json' }} />);
    expect(screen.getByText('{oops: not json')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Tree' })).toBeNull();
  });

  it('renders big ints in documents as numbers, not quoted strings', () => {
    const value: Value = {
      t: 'document',
      v: { id: { t: 'int', v: '9007199254740993' }, name: { t: 'text', v: 'x' } },
    };
    renderWithProviders(<ValueView value={value} />);
    const num = screen.getByText('9007199254740993');
    expect(num.className).toContain('text-warning');
    expect(screen.getByText('"x"').className).toContain('text-success');
  });
});

describe('JsonTree expand limit', () => {
  it('caps the number of nodes opened by expand all', () => {
    const wide = { items: Array.from({ length: 20 }, (_, i) => ({ a: { b: i } })) };
    const { paths, truncated } = collectExpandPaths(wide, 10);
    expect(paths.size).toBe(10);
    expect(truncated).toBe(true);
    expect(collectExpandPaths(wide).truncated).toBe(false);
  });
});

describe('CellViewer', () => {
  it('follows the selected cell', () => {
    const tab = 'table:test:viewer';
    const model = new GridModel({ sessionId: 's', table: { segments: [] } });
    model.columns = [
      { name: 'a', typeName: 'text', valueKind: 'text' },
      { name: 'b', typeName: 'text', valueKind: 'text' },
    ];
    model.rows = [[{ t: 'text', v: 'first' }, { t: 'null' }]];
    useDataUiStore.getState().patch(tab, { viewerOpen: true, selection: { row: 0, col: 0 } });
    renderWithProviders(<CellViewer tabId={tab} model={model} />);
    expect(screen.getByText('first')).toBeInTheDocument();
    expect(screen.getByText('a')).toBeInTheDocument();
  });
});

describe('CellViewer save to file', () => {
  const tab = 'table:test:save';
  const png = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3];

  function setup(value: Value, row = 2) {
    const model = new GridModel({
      sessionId: 's',
      table: { segments: [{ kind: 'table', name: 'events' }] },
    });
    model.columns = [{ name: 'blob', typeName: 'bytea', valueKind: 'bytes' }];
    model.rows = Array.from({ length: row + 1 }, () => [value]);
    useDataUiStore.getState().patch(tab, { viewerOpen: true, selection: { row, col: 0 } });
    const save = vi.fn();
    (ipc as unknown as { saveCellValue: unknown }).saveCellValue = save;
    renderWithProviders(<CellViewer tabId={tab} model={model} />);
    return save;
  }

  afterEach(() => {
    useToastStore.setState({ toasts: [] });
  });

  it('saves bytes with a name from table, column, row and the detected extension', async () => {
    const user = userEvent.setup();
    const save = setup({ t: 'bytes', v: b64(png) });
    save.mockResolvedValue('/tmp/events_blob_3.png');
    await user.click(screen.getByRole('button', { name: /save to file/i }));
    expect(save).toHaveBeenCalledWith('events_blob_3.png', b64(png));
    await waitFor(() =>
      expect(useToastStore.getState().toasts[0]).toMatchObject({
        tone: 'success',
        message: expect.stringContaining('/tmp/events_blob_3.png'),
      }),
    );
  });

  it('uses .bin when the type is not recognized and stays silent when the user cancels', async () => {
    const user = userEvent.setup();
    const save = setup({ t: 'bytes', v: b64([1, 2, 3, 4]) });
    save.mockResolvedValue(null);
    await user.click(screen.getByRole('button', { name: /save to file/i }));
    expect(save.mock.calls[0]![0]).toBe('events_blob_3.bin');
    await Promise.resolve();
    expect(useToastStore.getState().toasts).toHaveLength(0);
  });

  it('shows an error toast when saving fails', async () => {
    const user = userEvent.setup();
    const save = setup({ t: 'bytes', v: b64(png) });
    save.mockRejectedValue({ kind: 'io', message: 'disk full' });
    await user.click(screen.getByRole('button', { name: /save to file/i }));
    await waitFor(() =>
      expect(useToastStore.getState().toasts[0]).toMatchObject({
        tone: 'error',
        message: expect.stringContaining('disk full'),
      }),
    );
  });

  it('saves long text as UTF-8 .txt and long json as .json; short text has no button', async () => {
    const user = userEvent.setup();
    const text = `😀${'x'.repeat(1200)}`;
    const save = setup({ t: 'text', v: text });
    save.mockResolvedValue('/p');
    await user.click(screen.getByRole('button', { name: /save to file/i }));
    expect(save.mock.calls[0]![0]).toBe('events_blob_3.txt');
    const bytes = Uint8Array.from(atob(save.mock.calls[0]![1] as string), (c) => c.charCodeAt(0));
    expect(new TextDecoder().decode(bytes)).toBe(text);
  });

  it('offers no save button for short text', () => {
    setup({ t: 'text', v: 'short' });
    expect(screen.queryByRole('button', { name: /save to file/i })).toBeNull();
  });
});

describe('timestamp in the viewer', () => {
  it('shows both the UTC and the local form', () => {
    renderWithProviders(
      <ValueView value={{ t: 'timestamp', v: '2026-11-13T15:02:10.5Z' }} timestamp="local" />,
    );
    expect(screen.getByText('UTC / as stored')).toBeInTheDocument();
    expect(screen.getByText('Local time')).toBeInTheDocument();
    expect(screen.getByText(/2026-11-13T15:02:10.5Z/)).toBeInTheDocument();
    expect(
      screen.getByText(
        /2026-11-1[34] \d\d:02:10\.5[+-]\d\d:\d\d|2026-11-1[34] \d\d:\d\d:10\.5[+-]\d\d:\d\d/,
      ),
    ).toBeInTheDocument();
  });
});

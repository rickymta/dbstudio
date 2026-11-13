import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, describe, expect, it } from 'vitest';
import type { Value } from '@/lib/types';
import { CellViewer, ValueView } from './CellViewer';
import { GridModel } from './gridModel';
import { resetDataStores, useDataUiStore } from './store';
import { renderWithProviders } from './testUtils';

afterEach(() => resetDataStores());

const b64 = (bytes: number[]) => btoa(String.fromCharCode(...bytes));

describe('ValueView', () => {
  it('renders JSON as a collapsible tree and as pretty text', async () => {
    const user = userEvent.setup();
    const value: Value = { t: 'json', v: { n: 7, tags: ['a', 'b'], nested: { deep: { x: 1 } } } };
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

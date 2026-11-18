import { render } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { matchRanges } from '@/lib/text';
import { Highlight } from './Highlight';

describe('Highlight', () => {
  it('finds every case-insensitive occurrence', () => {
    expect(matchRanges('Order_ORDERS', 'order')).toEqual([
      [0, 5],
      [6, 11],
    ]);
    expect(matchRanges('users', '  ')).toEqual([]);
    expect(matchRanges('users', 'x')).toEqual([]);
  });

  it('marks the matches and keeps the rest of the text', () => {
    const { container } = render(<Highlight text="sp_get_orders" query="ORDER" />);
    expect(container.textContent).toBe('sp_get_orders');
    expect([...container.querySelectorAll('mark')].map((m) => m.textContent)).toEqual(['order']);
  });

  it('renders plain text without a query', () => {
    const { container } = render(<Highlight text="users" />);
    expect(container.querySelector('mark')).toBeNull();
    expect(container.textContent).toBe('users');
  });
});

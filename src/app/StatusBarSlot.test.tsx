import { render } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';
import { StatusBarSlot } from './StatusBarSlot';
import { statusBarSlots } from './statusBarSlots';

const original = statusBarSlots.jobs;
afterEach(() => {
  statusBarSlots.jobs = original;
});

describe('StatusBarSlot', () => {
  it('renders the jobs indicator button', () => {
    const { container } = render(<StatusBarSlot name="jobs" />);
    expect(container.querySelector('[data-testid="jobs-indicator"]')).not.toBeNull();
  });

  it('renders nothing without a registered renderer and the renderer when present', () => {
    statusBarSlots.jobs = undefined;
    const { container, rerender } = render(<StatusBarSlot name="jobs" />);
    expect(container).toBeEmptyDOMElement();
    statusBarSlots.jobs = () => <span data-testid="jobs">3 jobs</span>;
    rerender(<StatusBarSlot name="jobs" />);
    expect(container.querySelector('[data-testid="jobs"]')).not.toBeNull();
  });
});

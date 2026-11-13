import type { TableTabPayload } from '@/stores/tabs';

/** Placeholder: the data grid is implemented by the data frontend agent. */
export function TableTab({ path }: TableTabPayload) {
  return <div data-testid="table-tab">TableTab {path.segments.at(-1)?.name}</div>;
}

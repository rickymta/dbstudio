import type { DdlTabPayload } from '@/stores/tabs';

/** Placeholder: the DDL viewer is implemented by the data frontend agent. */
export function DdlTab({ path }: DdlTabPayload) {
  return <div data-testid="ddl-tab">DdlTab {path.segments.at(-1)?.name}</div>;
}

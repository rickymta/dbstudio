export interface ObjectTreeProps {
  connectionId: string;
}

/** Placeholder: the lazy object tree is implemented by the explorer frontend agent. */
export function ObjectTree({ connectionId }: ObjectTreeProps) {
  return <div data-testid="object-tree">ObjectTree {connectionId}</div>;
}

// In-memory event bus standing in for Tauri events / the bridge `/events` stream.
// Mocks call `mockEmit('job://progress', event)`; `ipc.onEvent` subscribes through `mockListen`.

type Handler = (payload: unknown) => void;

const listeners = new Map<string, Set<Handler>>();

export function mockEmit(name: string, payload: unknown): void {
  for (const handler of [...(listeners.get(name) ?? [])]) handler(payload);
}

/** Subscribe; the returned function unsubscribes. */
export function mockListen(name: string, handler: Handler): () => void {
  let set = listeners.get(name);
  if (!set) listeners.set(name, (set = new Set()));
  set.add(handler);
  return () => {
    set.delete(handler);
  };
}

export function resetMockEvents(): void {
  listeners.clear();
}

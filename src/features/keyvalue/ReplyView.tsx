const MAX_LINES = 500;

/** Typed reply of a Redis command, drawn like `redis-cli` prints it. */
export function ReplyView({ lines }: { lines: string[] }) {
  return (
    <pre
      className="font-mono text-xs break-words whitespace-pre-wrap select-text"
      data-testid="redis-reply"
    >
      {lines.slice(0, MAX_LINES).join('\n')}
      {lines.length > MAX_LINES && `\n... (${lines.length - MAX_LINES})`}
    </pre>
  );
}

// redis-cli style text of a typed Redis reply (the console shows replies like `redis-cli` does).

import type { Value } from '@/lib/types';
import { bytesToHex, base64ToBytes, decodeUtf8, valueText } from './valueFormat';

const quoted = (s: string) => JSON.stringify(s);

/** Lines of `value`; nested arrays/maps are indented under their numbered parent line. */
export function formatReply(value: Value, indent = 0): string[] {
  const pad = ' '.repeat(indent);
  switch (value.t) {
    case 'null':
      return [`${pad}(nil)`];
    case 'bool':
      return [`${pad}(${value.v ? 'true' : 'false'})`];
    case 'int':
    case 'uInt':
      return [`${pad}(integer) ${value.v}`];
    case 'float':
    case 'decimal':
      return [`${pad}(double) ${value.v}`];
    case 'bytes': {
      const bytes = base64ToBytes(value.v);
      return [
        `${pad}${decodeUtf8(bytes) !== null ? quoted(decodeUtf8(bytes)!) : `0x${bytesToHex(bytes)}`}`,
      ];
    }
    case 'array': {
      if (value.v.length === 0) return [`${pad}(empty array)`];
      const width = String(value.v.length).length;
      return value.v.flatMap((item, i) =>
        nested(`${String(i + 1).padStart(width)}) `, item, indent),
      );
    }
    case 'document': {
      if (value.v.length === 0) return [`${pad}(empty hash)`];
      const width = String(value.v.length).length;
      return value.v.flatMap(([key, item], i) =>
        nested(`${String(i + 1).padStart(width)}# ${quoted(key)} => `, item, indent),
      );
    }
    default:
      return [`${pad}${quoted(valueText(value))}`];
  }
}

/** `prefix` + first line of the child on one line, the rest aligned below it. */
function nested(prefix: string, item: Value, indent: number): string[] {
  const inner = formatReply(item, 0);
  const pad = ' '.repeat(indent);
  const align = ' '.repeat(prefix.length);
  return inner.map((line, i) => (i === 0 ? `${pad}${prefix}${line}` : `${pad}${align}${line}`));
}

/** All lines of a reply table: one block per row (a Redis result has a single column). */
export function formatReplyRows(rows: Value[][]): string[] {
  return rows.flatMap((row) => row.flatMap((cell) => formatReply(cell)));
}

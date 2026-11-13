// Display of `timestamp` (timestamptz) values in the machine's local time zone.
// Only the date-time part is shifted; the fractional seconds stay the original text, so
// microseconds are never rounded through `Date`.

export type TimestampDisplay = 'utc' | 'local';

/** UTC offset in minutes (east positive) of the local zone at the instant `epochMs`. */
export type OffsetFor = (epochMs: number) => number;

export const systemOffsetFor: OffsetFor = (ms) => -new Date(ms).getTimezoneOffset();

const RE =
  /^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2}):(\d{2})(\.\d+)?\s*(Z|[+-]\d{2}(?::?\d{2})?)?$/i;

const pad = (n: number, len = 2) => String(n).padStart(len, '0');

function parseOffsetMinutes(s: string | undefined): number {
  if (!s || s.toUpperCase() === 'Z') return 0;
  const sign = s[0] === '-' ? -1 : 1;
  const digits = s.slice(1).replace(':', '');
  const h = Number(digits.slice(0, 2));
  const m = digits.length > 2 ? Number(digits.slice(2, 4)) : 0;
  return sign * (h * 60 + m);
}

export function formatOffset(minutes: number): string {
  const rounded = Math.round(minutes);
  const abs = Math.abs(rounded);
  return `${rounded < 0 ? '-' : '+'}${pad(Math.floor(abs / 60))}:${pad(abs % 60)}`;
}

/**
 * `2026-11-13T15:02:10.123456Z` -> `2026-11-13 22:02:10.123456+07:00` for a +07:00 zone.
 * Strings that do not parse (`infinity`, years outside 1000-9999, ...) are returned unchanged.
 * A string without an offset is taken as UTC.
 */
export function toLocalTimestamp(raw: string, offsetFor: OffsetFor = systemOffsetFor): string {
  const m = RE.exec(raw.trim());
  if (!m) return raw;
  const [, y, mo, d, h, mi, s, frac, off] = m;
  const year = Number(y);
  if (year < 1000) return raw;
  const wall = Date.UTC(year, Number(mo) - 1, Number(d), Number(h), Number(mi), Number(s));
  const probe = new Date(wall);
  if (
    Number.isNaN(wall) ||
    probe.getUTCMonth() !== Number(mo) - 1 ||
    probe.getUTCDate() !== Number(d) ||
    probe.getUTCHours() !== Number(h)
  ) {
    return raw; // e.g. month 13, day 31 in a 30-day month
  }
  const instant = wall - parseOffsetMinutes(off) * 60_000;
  const offset = offsetFor(instant);
  const local = new Date(instant + offset * 60_000);
  if (
    Number.isNaN(local.getTime()) ||
    local.getUTCFullYear() < 1000 ||
    local.getUTCFullYear() > 9999
  ) {
    return raw;
  }
  return (
    `${pad(local.getUTCFullYear(), 4)}-${pad(local.getUTCMonth() + 1)}-${pad(local.getUTCDate())} ` +
    `${pad(local.getUTCHours())}:${pad(local.getUTCMinutes())}:${pad(local.getUTCSeconds())}` +
    `${frac ?? ''}${formatOffset(offset)}`
  );
}

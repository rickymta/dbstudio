import { afterEach, describe, expect, it, vi } from 'vitest';
import type { Value } from '@/lib/types';
import { formatCell, valueToText } from './format';
import { toLocalTimestamp, type OffsetFor } from './timestamp';

const fixed =
  (minutes: number): OffsetFor =>
  () =>
    minutes;

// US Eastern 2026: DST starts 2026-03-08 07:00:00Z (EST -300 -> EDT -240), ends 2026-11-01 06:00Z.
const eastern: OffsetFor = (ms) => {
  const start = Date.UTC(2026, 2, 8, 7);
  const end = Date.UTC(2026, 10, 1, 6);
  return ms >= start && ms < end ? -240 : -300;
};

describe('toLocalTimestamp', () => {
  it('shifts the date-time and writes an explicit offset, keeping fractional seconds verbatim', () => {
    expect(toLocalTimestamp('2026-11-13T15:02:10.123456Z', fixed(420))).toBe(
      '2026-11-13 22:02:10.123456+07:00',
    );
    expect(toLocalTimestamp('2026-11-13 15:02:10.1000000009+00', fixed(420))).toBe(
      '2026-11-13 22:02:10.1000000009+07:00',
    );
  });

  it('crosses the date boundary and handles half-hour and negative offsets', () => {
    expect(toLocalTimestamp('2026-01-01T20:00:00Z', fixed(330))).toBe('2026-01-02 01:30:00+05:30');
    expect(toLocalTimestamp('2026-01-01T02:10:00Z', fixed(-210))).toBe('2025-12-31 22:40:00-03:30');
    expect(toLocalTimestamp('2026-01-01T00:00:00Z', fixed(345))).toBe('2026-01-01 05:45:00+05:45');
  });

  it('converts from the offset present in the input, not only from Z', () => {
    expect(toLocalTimestamp('2026-11-13 10:00:00.5+05:30', fixed(0))).toBe(
      '2026-11-13 04:30:00.5+00:00',
    );
    expect(toLocalTimestamp('2026-11-13 10:00:00-0800', fixed(0))).toBe(
      '2026-11-13 18:00:00+00:00',
    );
    expect(toLocalTimestamp('2026-11-13 10:00:00+07', fixed(0))).toBe('2026-11-13 03:00:00+00:00');
  });

  it('uses the offset in force at that instant across DST changes', () => {
    // Just before and after the spring-forward instant (07:00:00Z).
    expect(toLocalTimestamp('2026-03-08T06:59:59.999999Z', eastern)).toBe(
      '2026-03-08 01:59:59.999999-05:00',
    );
    expect(toLocalTimestamp('2026-03-08T07:00:00Z', eastern)).toBe('2026-03-08 03:00:00-04:00');
    // Fall back: 05:59:59Z is still EDT, 06:00:00Z is EST (01:00 happens twice).
    expect(toLocalTimestamp('2026-11-01T05:59:59Z', eastern)).toBe('2026-11-01 01:59:59-04:00');
    expect(toLocalTimestamp('2026-11-01T06:00:00Z', eastern)).toBe('2026-11-01 01:00:00-05:00');
  });

  it('keeps strings it cannot parse or shift safely', () => {
    for (const raw of [
      'infinity',
      '-infinity',
      '',
      'not a date',
      '0001-01-01T00:00:00Z',
      '2026-13-01T00:00:00Z',
      '2026-02-31T00:00:00Z',
      '2026-11-13',
    ]) {
      expect(toLocalTimestamp(raw, fixed(420))).toBe(raw);
    }
    expect(toLocalTimestamp('9999-12-31T23:59:59Z', fixed(420))).toBe('9999-12-31T23:59:59Z');
  });
});

describe('timestamp display in cells', () => {
  afterEach(() => vi.unstubAllEnvs());

  const ts: Value = { t: 'timestamp', v: '2026-11-13T15:02:10.123456Z' };

  it('utc keeps the server string; other kinds never change', () => {
    expect(formatCell(ts).text).toBe('2026-11-13T15:02:10.123456Z');
    expect(formatCell(ts, undefined, 'utc').text).toBe('2026-11-13T15:02:10.123456Z');
    vi.stubEnv('TZ', 'Asia/Kolkata');
    for (const t of ['dateTime', 'date', 'time'] as const) {
      const v = { t, v: '2026-11-13T15:02:10' } as Value;
      expect(formatCell(v, undefined, 'local').text).toBe('2026-11-13T15:02:10');
    }
  });

  it('local converts to the machine zone (half-hour zone) and copy follows the display', () => {
    vi.stubEnv('TZ', 'Asia/Kolkata');
    expect(formatCell(ts, undefined, 'local').text).toBe('2026-11-13 20:32:10.123456+05:30');
    expect(valueToText(ts, 'local')).toBe('2026-11-13 20:32:10.123456+05:30');
    expect(valueToText(ts, 'utc')).toBe('2026-11-13T15:02:10.123456Z');
    expect(formatCell({ t: 'timestamp', v: 'infinity' }, undefined, 'local').text).toBe('infinity');
  });
});

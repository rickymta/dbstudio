import { describe, expect, it } from 'vitest';
import {
  buildCron,
  DEFAULT_PRESET,
  describeCron,
  isCronShape,
  parseCron,
  scheduleText,
  type SchedulePreset,
} from './schedule';

const preset = (p: Partial<SchedulePreset>): SchedulePreset => ({ ...DEFAULT_PRESET, ...p });

describe('buildCron', () => {
  it('builds daily, weekly and monthly expressions', () => {
    expect(buildCron(preset({ kind: 'daily', hour: 2, minute: 30 }))).toBe('30 2 * * *');
    expect(buildCron(preset({ kind: 'weekly', hour: 23, minute: 0, weekdays: [5, 1, 1] }))).toBe(
      '0 23 * * 1,5',
    );
    expect(buildCron(preset({ kind: 'monthly', hour: 4, minute: 15, day: 28 }))).toBe(
      '15 4 28 * *',
    );
  });
});

describe('parseCron', () => {
  it('round-trips presets', () => {
    for (const p of [
      preset({ kind: 'daily', hour: 3, minute: 5 }),
      preset({ kind: 'weekly', hour: 1, minute: 0, weekdays: [0, 3, 6] }),
      preset({ kind: 'monthly', hour: 12, minute: 59, day: 31 }),
    ]) {
      const parsed = parseCron(buildCron(p));
      expect(parsed && buildCron(parsed)).toBe(buildCron(p));
      expect(parsed?.kind).toBe(p.kind);
    }
  });

  it('maps Sunday written as 7 to 0', () => {
    expect(parseCron('0 1 * * 7,1')?.weekdays).toEqual([0, 1]);
  });

  it('returns null for expressions a preset cannot express', () => {
    for (const c of [
      '*/15 * * * *',
      '0 2 1 1 *',
      '0 2 1 * 1',
      '0 2 * * 1-5',
      '0 2 * *',
      '61 2 * * *',
    ]) {
      expect(parseCron(c)).toBeNull();
    }
  });
});

describe('isCronShape / describeCron', () => {
  it('checks the field count and characters', () => {
    expect(isCronShape('0 2 * * *')).toBe(true);
    expect(isCronShape(' */5  * * * * ')).toBe(true);
    expect(isCronShape('0 2 * *')).toBe(false);
    expect(isCronShape('0 2 * * * *')).toBe(false);
    expect(isCronShape('0 2 $ * *')).toBe(false);
  });

  it('describes in English and Vietnamese', () => {
    expect(describeCron('0 2 * * *', 'en')).toMatch(/02:00/);
    const vi = describeCron('0 2 * * *', 'vi');
    expect(vi).toBeTruthy();
    expect(vi).not.toBe(describeCron('0 2 * * *', 'en'));
  });

  it('returns null when the expression cannot be described', () => {
    expect(describeCron('nope', 'en')).toBeNull();
    expect(describeCron('99 99 * * *', 'en')).toBeNull();
  });

  it('scheduleText falls back to the raw cron or the manual label', () => {
    expect(scheduleText(null, 'en', 'Manual')).toBe('Manual');
    expect(scheduleText({ cron: '99 99 * * *' }, 'en', 'Manual')).toBe('99 99 * * *');
    expect(scheduleText({ cron: '0 2 * * *' }, 'en', 'Manual')).toMatch(/02:00/);
  });
});

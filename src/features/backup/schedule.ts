// Pure scheduling helpers: cron presets, cron <-> preset conversion, human description.

import cronstrue from 'cronstrue/i18n';

export type PresetKind = 'daily' | 'weekly' | 'monthly';

export interface SchedulePreset {
  kind: PresetKind;
  hour: number;
  minute: number;
  /** Weekly: 0 (Sunday) .. 6 (Saturday), sorted, unique. */
  weekdays: number[];
  /** Monthly: 1..31. */
  day: number;
}

export const DEFAULT_PRESET: SchedulePreset = {
  kind: 'daily',
  hour: 2,
  minute: 0,
  weekdays: [1],
  day: 1,
};

/** Build a 5-field cron expression from a preset. */
export function buildCron(p: SchedulePreset): string {
  const time = `${p.minute} ${p.hour}`;
  switch (p.kind) {
    case 'daily':
      return `${time} * * *`;
    case 'weekly': {
      const days = [...new Set(p.weekdays)].sort((a, b) => a - b);
      return `${time} * * ${days.length ? days.join(',') : '*'}`;
    }
    case 'monthly':
      return `${time} ${p.day} * *`;
  }
}

function int(text: string, min: number, max: number): number | null {
  if (!/^\d{1,2}$/.test(text)) return null;
  const n = Number(text);
  return n >= min && n <= max ? n : null;
}

/** Inverse of `buildCron`; `null` when the expression is not expressible as a preset. */
export function parseCron(cron: string): SchedulePreset | null {
  const f = cron.trim().split(/\s+/);
  if (f.length !== 5) return null;
  const [mi, ho, dom, mon, dow] = f as [string, string, string, string, string];
  const minute = int(mi, 0, 59);
  const hour = int(ho, 0, 23);
  if (minute === null || hour === null || mon !== '*') return null;
  if (dom === '*' && dow === '*') return { ...DEFAULT_PRESET, kind: 'daily', hour, minute };
  if (dom === '*') {
    const parts = dow.split(',').map((d) => int(d, 0, 7));
    if (parts.some((d) => d === null)) return null;
    const weekdays = [...new Set(parts.map((d) => (d as number) % 7))].sort((a, b) => a - b);
    return { ...DEFAULT_PRESET, kind: 'weekly', hour, minute, weekdays };
  }
  if (dow === '*') {
    const day = int(dom, 1, 31);
    return day === null ? null : { ...DEFAULT_PRESET, kind: 'monthly', hour, minute, day };
  }
  return null;
}

const CRON_FIELD = /^[\d*,/\-A-Za-z?LW#]+$/;

/** Shape check only (5 whitespace separated fields of cron characters); the backend validates. */
export function isCronShape(cron: string): boolean {
  const f = cron.trim().split(/\s+/);
  return f.length === 5 && f.every((x) => CRON_FIELD.test(x));
}

/** Human description of a cron expression in `language`; `null` when it cannot be described. */
export function describeCron(cron: string, language: string): string | null {
  if (!isCronShape(cron)) return null;
  try {
    return cronstrue.toString(cron.trim(), {
      locale: language.startsWith('vi') ? 'vi' : 'en',
      use24HourTimeFormat: true,
    });
  } catch {
    return null;
  }
}

/** Schedule of a job as text: cron description, or `manual` when there is no schedule. */
export function scheduleText(
  schedule: { cron: string } | null,
  language: string,
  manual: string,
): string {
  if (!schedule) return manual;
  return describeCron(schedule.cron, language) ?? schedule.cron;
}

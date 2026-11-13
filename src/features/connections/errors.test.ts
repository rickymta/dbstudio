import { describe, expect, it } from 'vitest';
import i18n from '@/lib/i18n';
import { describeError, stripKindPrefix } from './errors';
import { safeColor } from './color';
import { connectionColor } from './queries';
import { initialForm, profileFromForm } from './connectionForm';

describe('describeError', () => {
  const t = i18n.getFixedT('en');

  it('prefixes the translated kind label once', () => {
    expect(describeError(t, { kind: 'connection', message: 'refused' })).toBe(
      'Connection failed: refused',
    );
  });

  it('drops English kind prefixes that the backend still includes', () => {
    expect(describeError(t, { kind: 'connection', message: 'connection error: refused' })).toBe(
      'Connection failed: refused',
    );
    expect(
      describeError(t, { kind: 'authentication', message: 'Authentication failed: bad pw' }),
    ).toBe('Authentication failed: bad pw');
    expect(stripKindPrefix('tls error: handshake')).toBe('handshake');
    expect(stripKindPrefix('not found: group x')).toBe('group x');
  });

  it('keeps messages that merely start with a similar word', () => {
    const saved = 'connection saved, but its password could not be stored: keychain';
    expect(stripKindPrefix(saved)).toBe(saved);
    expect(stripKindPrefix('invalid data: name is empty')).toBe('invalid data: name is empty');
    expect(stripKindPrefix('connection error: ')).toBe('connection error: ');
  });
});

describe('safeColor', () => {
  it('accepts only #rgb and #rrggbb', () => {
    expect(safeColor('#e5484d')).toBe('#e5484d');
    expect(safeColor('#F0a')).toBe('#F0a');
    for (const bad of [
      '#f00, 0 0 0 100vmax #000',
      'red',
      '#ff',
      '#ffff',
      '#12345g',
      'var(--danger)',
      '#fff;',
      ' #fff',
      '',
      null,
      undefined,
    ]) {
      expect(safeColor(bad)).toBeNull();
    }
  });

  it('falls back to the environment color and never saves an invalid one', () => {
    const base = profileFromForm({ ...initialForm(undefined), name: 'x' }, undefined, () => 'id');
    const attack = '#f00, 0 0 0 100vmax #000';
    expect(connectionColor({ ...base, color: attack, environment: 'production' })).toBe(
      'var(--danger)',
    );
    expect(connectionColor({ ...base, color: attack, environment: null })).toBeUndefined();
    expect(initialForm({ ...base, color: attack }).color).toBeNull();
    const form = { ...initialForm(undefined), name: 'x', color: attack };
    expect(profileFromForm(form, undefined, () => 'id').color).toBeNull();
  });
});

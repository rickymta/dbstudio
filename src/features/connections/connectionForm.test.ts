import { describe, expect, it } from 'vitest';
import { autoName, initialForm, validateForm } from './connectionForm';

describe('autoName', () => {
  it('starts at 1 for the first connection of a kind', () => {
    expect(autoName('postgres', [])).toBe('postgres 1');
    expect(autoName('postgres', ['mysql 1', 'Production'])).toBe('postgres 1');
  });

  it('takes the first free number, ignoring case and spaces', () => {
    expect(autoName('postgres', ['postgres 1', ' POSTGRES 2 '])).toBe('postgres 3');
    expect(autoName('postgres', ['postgres 1', 'postgres 3'])).toBe('postgres 2');
  });

  it('only counts names of the form `<kind> N`', () => {
    expect(autoName('postgres', ['postgres', 'postgres 01', 'postgres 1a', 'postgresql 1'])).toBe(
      'postgres 1',
    );
  });
});

describe('validateForm', () => {
  it('accepts a blank name', () => {
    expect(validateForm({ ...initialForm(undefined), name: ' ' })).toEqual({});
  });
});

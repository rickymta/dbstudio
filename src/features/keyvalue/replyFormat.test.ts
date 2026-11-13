import { describe, expect, it } from 'vitest';
import type { Value } from '@/lib/types';
import { formatReply } from './replyFormat';

const text = (v: string): Value => ({ t: 'text', v });

describe('formatReply', () => {
  it('prints scalars like redis-cli', () => {
    expect(formatReply({ t: 'null' })).toEqual(['(nil)']);
    expect(formatReply({ t: 'int', v: 3 })).toEqual(['(integer) 3']);
    expect(formatReply(text('OK'))).toEqual(['"OK"']);
    expect(formatReply({ t: 'bytes', v: '/wA=' })).toEqual(['0xff00']);
    expect(formatReply({ t: 'bytes', v: 'aGk=' })).toEqual(['"hi"']);
  });

  it('numbers array items and aligns nested replies', () => {
    expect(formatReply({ t: 'array', v: [] })).toEqual(['(empty array)']);
    const nested: Value = {
      t: 'array',
      v: [text('a'), { t: 'array', v: [{ t: 'int', v: 1 }, { t: 'null' }] }],
    };
    expect(formatReply(nested)).toEqual(['1) "a"', '2) 1) (integer) 1', '   2) (nil)']);
  });

  it('prints maps with # markers', () => {
    expect(formatReply({ t: 'document', v: [['k', text('v')]] })).toEqual(['1# "k" => "v"']);
  });
});

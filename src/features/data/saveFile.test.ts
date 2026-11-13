import { describe, expect, it } from 'vitest';
import { detectFileExtension, suggestFileName, utf8ToBase64 } from './saveFile';

const b = (...x: number[]) => Uint8Array.from(x);

describe('saveFile helpers', () => {
  it('picks the extension from magic bytes', () => {
    expect(detectFileExtension(b(0x89, 0x50, 0x4e, 0x47))).toBe('png');
    expect(detectFileExtension(b(0xff, 0xd8, 0xff, 0xe0))).toBe('jpg');
    expect(detectFileExtension(new TextEncoder().encode('GIF89a'))).toBe('gif');
    expect(detectFileExtension(new TextEncoder().encode('RIFF\0\0\0\0WEBPVP8 '))).toBe('webp');
    expect(detectFileExtension(new TextEncoder().encode('%PDF-1.7'))).toBe('pdf');
    expect(detectFileExtension(b(0x50, 0x4b, 3, 4))).toBe('zip');
    expect(detectFileExtension(b(0x1f, 0x8b, 8))).toBe('gz');
    expect(detectFileExtension(b(1, 2, 3))).toBe('bin');
    expect(detectFileExtension(b())).toBe('bin');
  });

  it('suggests <table>_<column>_<row>.<ext> with safe characters', () => {
    expect(suggestFileName('events', 'blob', 3, 'png')).toBe('events_blob_3.png');
    expect(suggestFileName('my table', 'a/b:c', 1, 'bin')).toBe('my_table_a_b_c_1.bin');
    expect(suggestFileName('', '..', 2, 'txt')).toBe('value_..' + '_2.txt'.replace('..', ''));
  });

  it('encodes UTF-8 to base64 without breaking characters outside the BMP', () => {
    const text = 'héllo 😀 𝒳 日本語';
    const b64 = utf8ToBase64(text);
    const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
    expect(new TextDecoder().decode(bytes)).toBe(text);
    expect(utf8ToBase64('')).toBe('');
  });
});

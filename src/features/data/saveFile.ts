// Helpers for "Save to file": suggested names, extensions from magic bytes, UTF-8 -> base64.

export function detectFileExtension(bytes: Uint8Array): string {
  const at = (i: number) => bytes[i] ?? -1;
  if (at(0) === 0x89 && at(1) === 0x50 && at(2) === 0x4e && at(3) === 0x47) return 'png';
  if (at(0) === 0xff && at(1) === 0xd8 && at(2) === 0xff) return 'jpg';
  if (at(0) === 0x47 && at(1) === 0x49 && at(2) === 0x46 && at(3) === 0x38) return 'gif';
  if (
    at(0) === 0x52 &&
    at(1) === 0x49 &&
    at(2) === 0x46 &&
    at(3) === 0x46 &&
    at(8) === 0x57 &&
    at(9) === 0x45 &&
    at(10) === 0x42 &&
    at(11) === 0x50
  ) {
    return 'webp';
  }
  if (at(0) === 0x25 && at(1) === 0x50 && at(2) === 0x44 && at(3) === 0x46) return 'pdf';
  if (
    at(0) === 0x50 &&
    at(1) === 0x4b &&
    (at(2) === 3 || at(2) === 5) &&
    (at(3) === 4 || at(3) === 6)
  ) {
    return 'zip';
  }
  if (at(0) === 0x1f && at(1) === 0x8b) return 'gz';
  return 'bin';
}

const safe = (s: string) =>
  s.replace(/[^\p{L}\p{N}._-]+/gu, '_').replace(/^_+|_+$/g, '') || 'value';

/** `<table>_<column>_<row>.<ext>` with file-system-hostile characters replaced. */
export function suggestFileName(table: string, column: string, row: number, ext: string): string {
  return `${safe(table)}_${safe(column)}_${row}.${ext}`;
}

/** UTF-8 bytes of `text` as base64 (code points outside the BMP survive). */
export function utf8ToBase64(text: string): string {
  const bytes = new TextEncoder().encode(text);
  let binary = '';
  const CHUNK = 0x8000;
  for (let i = 0; i < bytes.length; i += CHUNK) {
    binary += String.fromCharCode(...bytes.subarray(i, i + CHUNK));
  }
  return btoa(binary);
}

/** Text values at least this long get a "Save to file" button. */
export const LONG_TEXT_CHARS = 1000;

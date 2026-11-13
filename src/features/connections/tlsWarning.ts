import type { TlsMode } from '@/lib/types';

/** True for hosts that never leave the machine: localhost, *.localhost, 127.0.0.0/8, ::1. */
export function isLoopbackHost(host: string): boolean {
  let h = host.trim().toLowerCase();
  if (h.startsWith('[') && h.endsWith(']')) h = h.slice(1, -1);
  h = h.replace(/\.$/, '');
  if (!h) return false;
  if (h === 'localhost' || h.endsWith('.localhost')) return true;
  if (h === '::1' || h === '0:0:0:0:0:0:0:1') return true;
  const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(h);
  if (!m) return false;
  const octets = m.slice(1).map(Number);
  return octets.every((o) => o <= 255) && octets[0] === 127;
}

/** TLS modes that do not verify the server certificate. */
const UNVERIFIED: ReadonlySet<TlsMode> = new Set(['disable', 'prefer', 'require']);

/** Whether to warn about man-in-the-middle exposure for this mode and host (empty host: no). */
export function needsTlsWarning(mode: TlsMode, host: string): boolean {
  return UNVERIFIED.has(mode) && host.trim() !== '' && !isLoopbackHost(host);
}

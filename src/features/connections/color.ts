const HEX_COLOR = /^#(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/;

/**
 * The only way a stored profile color may reach CSS. Accepts `#rgb` and `#rrggbb`; anything else
 * (imported files can contain arbitrary text) is dropped.
 */
export function safeColor(value: string | null | undefined): string | null {
  return typeof value === 'string' && HEX_COLOR.test(value) ? value : null;
}

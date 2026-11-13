// SQL formatting (QUERY-09) on top of sql-formatter, with the dialect of the connection.

import { format } from 'sql-formatter';
import type { ScriptDialect } from '@/lib/types';
import { formatterLanguageOf } from './dialects';

/**
 * Format `text`. Returns the input unchanged for non-SQL dialects, and `null` when the
 * formatter cannot parse the text (the caller tells the user instead of mangling the editor).
 */
export function formatSql(text: string, dialect: ScriptDialect): string | null {
  const language = formatterLanguageOf(dialect);
  if (language === null) return text;
  try {
    return format(text, {
      language,
      keywordCase: 'upper',
      tabWidth: 2,
      linesBetweenQueries: 1,
      // `:name`, `$1`, `?`, `@p` stay as written (QUERY-10).
      paramTypes: { named: [':'], numbered: ['$', '?'], custom: [{ regex: String.raw`@\w+` }] },
    });
  } catch {
    return null;
  }
}

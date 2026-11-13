// ScriptDialect -> CodeMirror language and sql-formatter language (QUERY-02, QUERY-09).

import { javascript } from '@codemirror/lang-javascript';
import {
  MariaSQL,
  MSSQL,
  MySQL,
  PLSQL,
  PostgreSQL,
  StandardSQL,
  sql,
  type SQLDialect,
} from '@codemirror/lang-sql';
import type { Extension } from '@codemirror/state';
import type { SqlLanguage } from 'sql-formatter';
import type { ScriptDialect } from '@/lib/types';
import type { EditorMode } from '@/stores/tabs';

export function sqlDialectOf(dialect: ScriptDialect): SQLDialect {
  switch (dialect.name) {
    case 'postgres':
      return PostgreSQL;
    case 'mySql':
      return dialect.mariadb ? MariaSQL : MySQL;
    case 'msSql':
      return MSSQL;
    case 'oracle':
      return PLSQL;
    default:
      return StandardSQL;
  }
}

/** sql-formatter language of a dialect; `null` when the text is not SQL (Mongo, Redis). */
export function formatterLanguageOf(dialect: ScriptDialect): SqlLanguage | null {
  switch (dialect.name) {
    case 'postgres':
      return 'postgresql';
    case 'mySql':
      return dialect.mariadb ? 'mariadb' : 'mysql';
    case 'msSql':
      return 'transactsql';
    case 'oracle':
      return 'plsql';
    case 'opaque':
      return 'sql';
    default:
      return null;
  }
}

/** Highlighting extension for the editor mode. Redis commands are plain text. */
export function languageExtension(mode: EditorMode, dialect: ScriptDialect): Extension {
  if (mode === 'mongo') return javascript();
  if (mode === 'redis') return [];
  return sql({ dialect: sqlDialectOf(dialect), upperCaseKeywords: true });
}

/** Whether the editor mode has an SQL-style statement format. */
export function isSqlDialect(dialect: ScriptDialect): boolean {
  return formatterLanguageOf(dialect) !== null;
}

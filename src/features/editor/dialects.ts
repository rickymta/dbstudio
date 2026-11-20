// ScriptDialect -> CodeMirror language and sql-formatter language (QUERY-02, QUERY-09).

import { javascript } from '@codemirror/lang-javascript';
import {
  MariaSQL,
  MSSQL,
  MySQL,
  PLSQL,
  PostgreSQL,
  SQLDialect,
  StandardSQL,
  sql,
} from '@codemirror/lang-sql';
import type { Extension } from '@codemirror/state';
import type { SqlLanguage } from 'sql-formatter';
import type { ScriptDialect } from '@/lib/types';
import type { EditorMode } from '@/stores/tabs';

/** CQL (Cassandra, ScyllaDB): standard SQL words plus the CQL statements and types. */
const CQL = SQLDialect.define({
  ...StandardSQL.spec,
  slashComments: true,
  keywords:
    (StandardSQL.spec.keywords ?? '') +
    ' aggregate allow apply batch clustering compact consistency contains counter custom' +
    ' describe entries filtering finalfunc frozen if initcond json keys keyspace keyspaces' +
    ' logged login materialized nologin norecursive nosuperuser password permission' +
    ' permissions replication sfunc static storage stype superuser token tracing truncate' +
    ' ttl unlogged use writetime',
  types:
    'ascii bigint blob boolean counter date decimal double duration float inet int list map' +
    ' set smallint text time timestamp timeuuid tinyint tuple uuid varchar varint vector',
});

/** ClickHouse: back-quoted identifiers, `#` comments and the engine / format vocabulary. */
const ClickHouse = SQLDialect.define({
  ...StandardSQL.spec,
  hashComments: true,
  slashComments: true,
  identifierQuotes: '`"',
  backslashEscapes: true,
  keywords:
    (StandardSQL.spec.keywords ?? '') +
    ' alias anti any asof attach cluster codec database databases detach dictionaries' +
    ' dictionary engine explain final format global ilike kill limit live materialized' +
    ' mutation optimize partition populate prewhere projection rename replace sample' +
    ' semi settings show sync tables totals truncate ttl use view watch',
  types:
    'array bool date date32 datetime datetime64 decimal decimal32 decimal64 decimal128' +
    ' enum8 enum16 fixedstring float32 float64 int8 int16 int32 int64 int128 int256 ipv4' +
    ' ipv6 json lowcardinality map nested nullable string tuple uint8 uint16 uint32 uint64' +
    ' uint128 uint256 uuid',
});

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
    case 'cql':
      return CQL;
    case 'clickHouse':
      return ClickHouse;
    default:
      return StandardSQL;
  }
}

/**
 * sql-formatter language of a dialect; `null` when the text is not SQL (Mongo, Redis) or the
 * formatter has no grammar for it (CQL).
 */
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
    case 'clickHouse':
      return 'clickhouse';
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

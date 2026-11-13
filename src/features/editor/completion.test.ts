import { describe, expect, it } from 'vitest';
import {
  completionsAt,
  consoleCompletionsAt,
  parseTableRefs,
  referencedTables,
  tokenize,
  type Catalog,
} from './completion';

const catalog: Catalog = {
  defaultSchema: 'public',
  tables: [
    {
      schema: 'public',
      name: 'users',
      kind: 'table',
      columns: [
        { name: 'id', typeName: 'int4' },
        { name: 'email', typeName: 'text' },
        { name: 'Created At', typeName: 'timestamptz' },
      ],
    },
    {
      schema: 'public',
      name: 'orders',
      kind: 'table',
      columns: [
        { name: 'id', typeName: 'int4' },
        { name: 'user_id', typeName: 'int4' },
        { name: 'total', typeName: 'numeric' },
      ],
    },
    {
      schema: 'audit',
      name: 'log',
      kind: 'table',
      columns: [{ name: 'message', typeName: 'text' }],
    },
  ],
};

/** `|` marks the cursor. */
function at(source: string, cat: Catalog = catalog) {
  const pos = source.indexOf('|');
  const text = source.replace('|', '');
  return completionsAt({
    text,
    pos,
    catalog: cat,
    vocabulary: { keywords: ['SELECT', 'FROM', 'WHERE'], functions: ['count'] },
  });
}

const labels = (r: ReturnType<typeof at>, type?: string): string[] =>
  (r?.options ?? []).filter((o) => !type || o.type === type).map((o) => o.label);

describe('completion with aliases', () => {
  it('suggests the columns of an alias: SELECT u.| FROM users u', () => {
    const r = at('SELECT u.| FROM users u');
    expect(labels(r)).toEqual(['id', 'email', 'Created At']);
    expect(r!.from).toBe('SELECT u.'.length);
  });

  it('filters by prefix through `from` and resolves AS aliases', () => {
    const r = at('SELECT o.us| FROM users AS u JOIN orders AS o ON o.user_id = u.id');
    expect(labels(r)).toEqual(['id', 'user_id', 'total']);
    expect(r!.from).toBe('SELECT o.'.length);
  });

  it('resolves several aliases and quoted column names', () => {
    const r = at('SELECT u.id, o.total FROM users u, orders o WHERE u.|');
    expect(labels(r)).toContain('email');
    const created = r!.options.find((o) => o.label === 'Created At');
    expect(created?.apply).toBe('"Created At"');
  });

  it('uses the table name when it has no alias and handles schema-qualified refs', () => {
    expect(labels(at('SELECT users.| FROM users'))).toContain('email');
    expect(labels(at('SELECT l.| FROM audit.log l'))).toEqual(['message']);
    expect(labels(at('SELECT * FROM audit.log WHERE audit.log.|'))).toEqual(['message']);
  });

  it('lists the tables of a schema after `schema.`', () => {
    expect(labels(at('SELECT * FROM audit.|'))).toEqual(['log']);
  });

  it('only looks at the statement that contains the cursor', () => {
    const r = at('SELECT x FROM orders o; SELECT u.| FROM users u; SELECT 1');
    expect(labels(r)).toContain('email');
    expect(labels(r)).not.toContain('total');
  });

  it('offers tables (not columns) after FROM and columns of the referenced tables elsewhere', () => {
    const afterFrom = at('SELECT * FROM |');
    expect(labels(afterFrom, 'table')).toEqual(['users', 'orders', 'log']);
    expect(labels(afterFrom, 'column')).toEqual([]);
    expect(labels(afterFrom, 'keyword')).toEqual([]);

    const where = at('SELECT * FROM users u WHERE |');
    expect(labels(where, 'column')).toContain('email');
    expect(labels(where, 'alias')).toEqual(['u']);
    expect(labels(where, 'keyword')).toContain('SELECT');
    expect(labels(where, 'function')).toContain('count');
  });

  it('gives no completions inside strings and comments', () => {
    expect(at("SELECT 'ab| FROM users")).toBeNull();
    expect(at('SELECT 1 -- note u.|')).toBeNull();
    expect(at('/* u.| */ SELECT 1')).toBeNull();
    expect(at("SELECT 'x' || u.| FROM users u")).not.toBeNull();
  });

  it('does not take a keyword for an alias', () => {
    const refs = parseTableRefs(tokenize('SELECT * FROM users WHERE id = 1'));
    expect(refs).toEqual([{ schema: null, name: 'users', alias: null }]);
    const joined = parseTableRefs(tokenize('SELECT * FROM users u LEFT JOIN orders ON 1=1'));
    expect(joined.map((r) => r.alias)).toEqual(['u', null]);
  });

  it('skips sub-selects and function calls when collecting references', () => {
    const refs = parseTableRefs(
      tokenize('SELECT * FROM (SELECT 1) s JOIN generate_series(1, 3) g ON true JOIN orders o ON true'),
    );
    expect(refs).toEqual([{ schema: null, name: 'orders', alias: 'o' }]);
  });

  it('reports the tables to hydrate for the statement at the cursor', () => {
    const text = 'SELECT 1; SELECT u. FROM users u JOIN audit.log l';
    const refs = referencedTables(text, text.indexOf('u.') + 2);
    expect(refs.map((r) => r.name)).toEqual(['users', 'log']);
  });

  it('supports bracket quotes for T-SQL', () => {
    const tokens = tokenize('SELECT [my col] FROM [dbo].[t]', true);
    expect(tokens.filter((t) => t.kind === 'quoted').map((t) => t.value)).toEqual([
      'my col',
      'dbo',
      't',
    ]);
  });
});

describe('console completion', () => {
  const commands = [
    { name: 'GET', syntax: 'GET key', summary: '' },
    { name: 'SET', syntax: 'SET key value', summary: '' },
  ];
  it('completes Redis command names only at the start of a line', () => {
    const first = consoleCompletionsAt({
      mode: 'redis',
      text: 'GE',
      pos: 2,
      commands,
      collections: [],
    });
    expect(first?.options.map((o) => o.label)).toEqual(['GET', 'SET']);
    expect(
      consoleCompletionsAt({ mode: 'redis', text: 'GET k', pos: 5, commands, collections: [] }),
    ).toBeNull();
  });

  it('completes Mongo collections, methods and operators', () => {
    const base = { mode: 'mongo' as const, commands: [], collections: ['users', 'orders'] };
    expect(
      consoleCompletionsAt({ ...base, text: 'db.', pos: 3 })?.options.map((o) => o.label),
    ).toEqual(['users', 'orders']);
    expect(
      consoleCompletionsAt({ ...base, text: 'db.users.', pos: 9 })?.options.map((o) => o.label),
    ).toContain('find');
    expect(
      consoleCompletionsAt({ ...base, text: 'db.users.find({ a: { $', pos: 22 })?.options.map(
        (o) => o.label,
      ),
    ).toContain('$gt');
  });
});

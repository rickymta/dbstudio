// Mock schema model, ERD layouts (in memory) and DDL generation.

import { childPath, ROOT_PATH } from '../objectPath';
import type {
  ColumnDetail,
  DdlItem,
  DdlRequest,
  ErdLayout,
  ErdLayoutSummary,
  ObjectKind,
  ObjectPath,
  Relation,
  SchemaModel,
  SchemaModelRequest,
  TableModel,
  Uuid,
  ValueKind,
} from '../types';
import { mockSessionKind } from './sessions';
import { clone, ipcError, sleep } from './util';

let layouts: ErdLayout[] = [];

// ---------------------------------------------------------------------------------------------
// Sample schema (a small Pagila-like database) and a generator for big ones
// ---------------------------------------------------------------------------------------------

export interface ColSpec {
  name: string;
  type: string;
  /** Nullable column. */
  null?: boolean;
  /** Part of the primary key. */
  pk?: boolean;
  /** `table.column` referenced by a foreign key. */
  fk?: string;
  /** The FK columns are also unique (1-1). */
  unique?: boolean;
}

export interface TableSpec {
  name: string;
  kind?: ObjectKind;
  comment?: string;
  cols: ColSpec[];
}

const id = (name: string, fk?: string, extra: Partial<ColSpec> = {}): ColSpec => ({
  name,
  type: 'integer',
  ...(fk ? { fk } : {}),
  ...extra,
});

const SAMPLE: TableSpec[] = [
  {
    name: 'country',
    cols: [id('country_id', undefined, { pk: true }), { name: 'country', type: 'varchar(50)' }],
  },
  {
    name: 'city',
    cols: [
      id('city_id', undefined, { pk: true }),
      { name: 'city', type: 'varchar(50)' },
      id('country_id', 'country.country_id'),
    ],
  },
  {
    name: 'address',
    cols: [
      id('address_id', undefined, { pk: true }),
      { name: 'address', type: 'varchar(50)' },
      { name: 'address2', type: 'varchar(50)', null: true },
      id('city_id', 'city.city_id'),
    ],
  },
  {
    name: 'language',
    cols: [id('language_id', undefined, { pk: true }), { name: 'name', type: 'char(20)' }],
  },
  {
    name: 'category',
    cols: [
      id('category_id', undefined, { pk: true }),
      { name: 'name', type: 'varchar(25)' },
      id('parent_category_id', 'category.category_id', { null: true }),
    ],
  },
  {
    name: 'actor',
    cols: [
      id('actor_id', undefined, { pk: true }),
      { name: 'first_name', type: 'varchar(45)' },
      { name: 'last_name', type: 'varchar(45)' },
    ],
  },
  {
    name: 'film',
    comment: 'Films; one row per title',
    cols: [
      id('film_id', undefined, { pk: true }),
      { name: 'title', type: 'varchar(255)' },
      { name: 'description', type: 'text', null: true },
      id('language_id', 'language.language_id'),
      id('original_language_id', 'language.language_id', { null: true }),
      { name: 'rental_rate', type: 'numeric(4,2)' },
      { name: 'length', type: 'smallint', null: true },
    ],
  },
  {
    name: 'film_actor',
    cols: [
      id('actor_id', 'actor.actor_id', { pk: true }),
      id('film_id', 'film.film_id', { pk: true }),
    ],
  },
  {
    name: 'film_category',
    cols: [
      id('film_id', 'film.film_id', { pk: true }),
      id('category_id', 'category.category_id', { pk: true }),
    ],
  },
  {
    name: 'film_text',
    cols: [
      id('film_id', 'film.film_id', { pk: true, unique: true }),
      { name: 'title', type: 'text' },
    ],
  },
  {
    name: 'store',
    cols: [
      id('store_id', undefined, { pk: true }),
      id('manager_staff_id', 'staff.staff_id', { unique: true }),
      id('address_id', 'address.address_id'),
    ],
  },
  {
    name: 'staff',
    cols: [
      id('staff_id', undefined, { pk: true }),
      { name: 'first_name', type: 'varchar(45)' },
      id('address_id', 'address.address_id'),
      id('store_id', 'store.store_id'),
      id('reports_to', 'staff.staff_id', { null: true }),
    ],
  },
  {
    name: 'customer',
    cols: [
      id('customer_id', undefined, { pk: true }),
      id('store_id', 'store.store_id'),
      { name: 'first_name', type: 'varchar(45)' },
      { name: 'email', type: 'varchar(50)', null: true },
      id('address_id', 'address.address_id'),
    ],
  },
  {
    name: 'inventory',
    cols: [
      id('inventory_id', undefined, { pk: true }),
      id('film_id', 'film.film_id'),
      id('store_id', 'store.store_id'),
    ],
  },
  {
    name: 'rental',
    cols: [
      id('rental_id', undefined, { pk: true }),
      { name: 'rental_date', type: 'timestamp' },
      id('inventory_id', 'inventory.inventory_id'),
      id('customer_id', 'customer.customer_id'),
      id('staff_id', 'staff.staff_id'),
    ],
  },
  {
    name: 'payment',
    cols: [
      id('payment_id', undefined, { pk: true }),
      id('customer_id', 'customer.customer_id'),
      id('staff_id', 'staff.staff_id'),
      id('rental_id', 'rental.rental_id', { null: true }),
      { name: 'amount', type: 'numeric(5,2)' },
    ],
  },
  {
    name: 'film_list',
    kind: 'view',
    cols: [id('film_id', undefined, { null: true }), { name: 'title', type: 'varchar(255)' }],
  },
];

/** `n` tables `t001..`: each one references the table at half its index (a binary tree). */
export function generateSpecs(n: number): TableSpec[] {
  const pad = (i: number) => `t${String(i).padStart(3, '0')}`;
  const specs: TableSpec[] = [];
  for (let i = 1; i <= n; i++) {
    const cols: ColSpec[] = [
      id('id', undefined, { pk: true }),
      { name: 'name', type: 'text' },
      { name: 'created_at', type: 'timestamp', null: true },
    ];
    if (i > 1) cols.push(id('parent_id', `${pad(Math.floor(i / 2))}.id`, { null: i % 3 === 0 }));
    if (i > 10 && i % 7 === 0) cols.push(id('other_id', `${pad(i - 5)}.id`, { null: true }));
    specs.push({ name: pad(i), cols });
  }
  return specs;
}

/** A generated schema model of `n` tables (see [`generateSpecs`]), for tests and demos. */
export function generateModel(n: number, database = 'bench'): SchemaModel {
  const scope = childPath(ROOT_PATH, 'database', database);
  return modelFromSpecs(generateSpecs(n), { kind: 'postgres', database, schema: 'public' }, scope, {
    includeViews: false,
    includeComments: false,
  });
}

/** The sample schema model of the mock PostgreSQL server. */
export function sampleModel(includeViews = true): SchemaModel {
  const scope = childPath(ROOT_PATH, 'database', 'app_dev');
  return modelFromSpecs(
    SAMPLE,
    { kind: 'postgres', database: 'app_dev', schema: 'public' },
    scope,
    { includeViews, includeComments: true },
  );
}

function valueKind(type: string): ValueKind {
  if (/^(integer|int|smallint|bigint)/.test(type)) return 'int';
  if (/^numeric/.test(type)) return 'decimal';
  if (/^timestamp/.test(type)) return 'timestamp';
  return 'text';
}

export interface Place {
  kind: 'postgres' | 'mysql';
  database: string;
  schema: string | null;
}

function placeOf(scope: ObjectPath, kind: string): Place {
  const database = scope.segments.find((s) => s.kind === 'database')?.name ?? 'app_dev';
  const schema = scope.segments.find((s) => s.kind === 'schema')?.name ?? null;
  const isMysql = kind === 'mysql';
  return {
    kind: isMysql ? 'mysql' : 'postgres',
    database,
    schema: isMysql ? null : (schema ?? 'public'),
  };
}

function pathOf(place: Place, spec: TableSpec): ObjectPath {
  const kind = spec.kind ?? 'table';
  const folder = kind === 'view' ? 'views' : 'tables';
  let p = childPath(ROOT_PATH, 'database', place.database);
  if (place.schema) {
    p = childPath(childPath(p, 'folder', 'schemas'), 'schema', place.schema);
  }
  return childPath(childPath(p, 'folder', folder), kind, spec.name);
}

/** Schema model of `specs` as the drivers would return it. */
export function modelFromSpecs(
  specs: TableSpec[],
  place: Place,
  scope: ObjectPath,
  req: Pick<SchemaModelRequest, 'includeViews' | 'includeComments'>,
): SchemaModel {
  const wanted = specs.filter((s) => (s.kind ?? 'table') === 'table' || req.includeViews);
  const byName = new Map(wanted.map((s) => [s.name, s]));
  const tables: TableModel[] = wanted.map((s) => {
    const pk = s.cols.filter((c) => c.pk).map((c) => c.name);
    const columns: ColumnDetail[] = s.cols.map((c, i) => ({
      name: c.name,
      ordinal: i + 1,
      typeName: c.type,
      valueKind: valueKind(c.type),
      nullable: c.null === true,
      default: null,
      comment: null,
      primaryKey: c.pk === true,
      extra: null,
    }));
    return {
      path: pathOf(place, s),
      kind: s.kind ?? 'table',
      schema: place.schema,
      name: s.name,
      comment: req.includeComments ? (s.comment ?? null) : null,
      columns,
      primaryKey: pk,
      uniqueKeys: s.cols.filter((c) => c.unique && !c.pk).map((c) => [c.name]),
      estimatedRows: 1000,
    };
  });
  const relations: Relation[] = [];
  for (const s of wanted) {
    for (const c of s.cols) {
      if (!c.fk) continue;
      const [t, col] = c.fk.split('.') as [string, string];
      const target = byName.get(t);
      if (!target) continue;
      relations.push({
        name: `${s.name}_${c.name}_fkey`,
        fromTable: pathOf(place, s),
        fromColumns: [c.name],
        toTable: pathOf(place, target),
        toColumns: [col],
        onUpdate: 'NO ACTION',
        onDelete: 'NO ACTION',
        fromOptional: c.null === true,
        oneToOne:
          c.pk === true && s.cols.filter((x) => x.pk).length === 1 ? true : c.unique === true,
        inferred: false,
      });
    }
  }
  return { scope, tables, relations, inferred: false, truncated: false };
}

function tableDdl(t: TableModel, place: Place): string {
  const q = (n: string) => (place.kind === 'mysql' ? `\`${n}\`` : `"${n}"`);
  const name = place.schema ? `${q(place.schema)}.${q(t.name)}` : q(t.name);
  const lines = t.columns.map(
    (c) => `    ${q(c.name)} ${c.typeName}${c.nullable ? '' : ' NOT NULL'}`,
  );
  if (t.primaryKey.length > 0) {
    lines.push(`    PRIMARY KEY (${t.primaryKey.map(q).join(', ')})`);
  }
  return `CREATE TABLE ${name} (\n${lines.join(',\n')}\n);`;
}

function lastName(path: ObjectPath): string {
  return path.segments[path.segments.length - 1]?.name ?? '';
}

// ---------------------------------------------------------------------------------------------

export const erdMock = {
  async schemaModel(sessionId: Uuid, request: SchemaModelRequest): Promise<SchemaModel> {
    const kind = mockSessionKind(sessionId);
    await sleep(30, 90);
    if (!request.scope.segments.some((s) => s.kind === 'database')) {
      return { scope: request.scope, tables: [], relations: [], inferred: false, truncated: false };
    }
    const place = placeOf(request.scope, kind);
    const model = modelFromSpecs(SAMPLE, place, request.scope, request);
    const names = new Set((request.tables ?? []).map(lastName));
    if (names.size === 0) return model;
    const tables = model.tables.filter((t) => names.has(t.name));
    const keep = new Set(tables.map((t) => t.name));
    return {
      ...model,
      tables,
      relations: model.relations.filter(
        (r) => keep.has(lastName(r.fromTable)) && keep.has(lastName(r.toTable)),
      ),
    };
  },
  async erdLayoutsList(connectionId: Uuid): Promise<ErdLayoutSummary[]> {
    return layouts
      .filter((l) => l.connectionId === connectionId)
      .sort((a, b) => b.updatedAt.localeCompare(a.updatedAt))
      .map((l) => ({
        id: l.id,
        name: l.name,
        scope: l.scope,
        tableCount: Array.isArray(l.tables) ? l.tables.length : 0,
        updatedAt: l.updatedAt,
      }));
  },
  async erdLayoutGet(id: Uuid): Promise<ErdLayout> {
    const layout = layouts.find((l) => l.id === id);
    if (!layout) throw ipcError('notFound', `layout ${id}`);
    return clone(layout);
  },
  async erdLayoutSave(layout: ErdLayout): Promise<ErdLayout> {
    const name = layout.name.trim();
    if (!name) throw ipcError('other', 'layout name is empty');
    if (
      layouts.some(
        (l) => l.id !== layout.id && l.connectionId === layout.connectionId && l.name === name,
      )
    ) {
      throw ipcError('other', `a layout named \`${name}\` already exists`);
    }
    const now = new Date().toISOString();
    const existing = layouts.find((l) => l.id === layout.id);
    if (existing && existing.updatedAt !== layout.updatedAt) {
      throw ipcError(
        'other',
        'layout-conflict: the layout was changed elsewhere since it was loaded',
      );
    }
    const saved: ErdLayout = {
      ...clone(layout),
      name,
      createdAt: existing?.createdAt ?? (layout.createdAt || now),
      updatedAt: now,
    };
    layouts = [...layouts.filter((l) => l.id !== layout.id), saved];
    return clone(saved);
  },
  async erdLayoutDelete(id: Uuid): Promise<void> {
    if (!layouts.some((l) => l.id === id)) throw ipcError('notFound', `layout ${id}`);
    layouts = layouts.filter((l) => l.id !== id);
  },
  async ddlGenerate(sessionId: Uuid, request: DdlRequest): Promise<DdlItem[]> {
    const kind = mockSessionKind(sessionId);
    if (!request.scope.segments.some((s) => s.kind === 'database')) return [];
    const place = placeOf(request.scope, kind);
    const model = modelFromSpecs(SAMPLE, place, request.scope, {
      includeViews: false,
      includeComments: false,
    });
    const last = request.scope.segments[request.scope.segments.length - 1];
    const only = new Set(
      [...(request.objects ?? []), ...(last?.kind === 'table' ? [request.scope] : [])].map(
        lastName,
      ),
    );
    const tables = model.tables.filter((t) => only.size === 0 || only.has(t.name));
    const items: DdlItem[] = tables.map((t) => ({
      path: t.path,
      kind: 'table',
      section: 'preData',
      sql: tableDdl(t, place),
      language: 'sql',
      dependsOn: [],
    }));
    const keep = new Set(tables.map((t) => t.name));
    for (const r of model.relations) {
      if (!keep.has(lastName(r.fromTable))) continue;
      const q = (n: string) => (place.kind === 'mysql' ? `\`${n}\`` : `"${n}"`);
      const full = (p: ObjectPath) =>
        place.schema ? `${q(place.schema)}.${q(lastName(p))}` : q(lastName(p));
      items.push({
        path: r.fromTable,
        kind: 'table',
        section: 'postData',
        sql: `ALTER TABLE ${full(r.fromTable)} ADD CONSTRAINT ${q(r.name)} FOREIGN KEY (${r.fromColumns.map(q).join(', ')}) REFERENCES ${full(r.toTable)} (${r.toColumns.map(q).join(', ')});`,
        language: 'sql',
        dependsOn: [r.fromTable, r.toTable],
      });
    }
    return items;
  },
};

export function resetMockErd(): void {
  layouts = [];
}

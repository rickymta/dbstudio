// Catalog and vocabulary of a session for autocomplete (QUERY-03). `schema_model` gives the
// whole catalog in a few queries; when a driver cannot (or returns nothing) the object tree is
// walked instead and columns are read lazily, per table, when a statement mentions it.

import { ipc } from '@/lib/ipc';
import { ROOT_PATH } from '@/lib/objectPath';
import type {
  ObjectKind,
  ObjectNode,
  ObjectPath,
  SchemaModel,
  TableModel,
  Uuid,
  Vocabulary,
} from '@/lib/types';
import {
  resolveTable,
  type Catalog,
  type CatalogTable,
  type CompletionVocabulary,
  type TableRef,
} from './completion';

const TABLE_KINDS: ReadonlySet<ObjectKind> = new Set([
  'table',
  'view',
  'materializedView',
  'collection',
]);
/** The tree is walked at most this deep below the scope, and at most this many tables are read. */
const MAX_DEPTH = 4;
const MAX_TABLES = 2000;
const CACHE_MS = 5 * 60_000;

export function tableFromModel(t: TableModel): CatalogTable {
  return {
    schema: t.schema,
    name: t.name,
    kind: t.kind,
    columns: t.columns.map((c) => ({ name: c.name, typeName: c.typeName })),
  };
}

function schemaOfPath(path: ObjectPath): string | null {
  return path.segments.find((s) => s.kind === 'schema')?.name ?? null;
}

/** Catalog of one database of a session; columns of tree-discovered tables load on demand. */
export class SessionCatalog {
  readonly catalog: Catalog;
  /** Tables found by walking the tree: their columns are not known yet. */
  private lazy = new Map<CatalogTable, ObjectPath>();
  private loading = new Map<CatalogTable, Promise<void>>();

  constructor(
    private readonly sessionId: Uuid,
    tables: CatalogTable[],
    lazy: Map<CatalogTable, ObjectPath>,
    defaultSchema: string | null,
  ) {
    this.catalog = { tables, defaultSchema };
    this.lazy = lazy;
  }

  /** Make sure the columns of the tables `refs` point to are loaded. */
  async hydrate(refs: readonly TableRef[]): Promise<void> {
    const jobs: Promise<void>[] = [];
    for (const ref of refs) {
      const table = resolveTable(this.catalog, ref.schema, ref.name);
      if (!table) continue;
      const path = this.lazy.get(table);
      if (!path) continue;
      let job = this.loading.get(table);
      if (!job) {
        job = ipc
          .tableDetails(this.sessionId, path)
          .then((d) => {
            table.columns = d.columns.map((c) => ({ name: c.name, typeName: c.typeName }));
            this.lazy.delete(table);
          })
          .catch(() => {
            // Leave the table without columns; completion still offers its name.
            this.lazy.delete(table);
          })
          .finally(() => this.loading.delete(table));
        this.loading.set(table, job);
      }
      jobs.push(job);
    }
    await Promise.all(jobs);
  }
}

async function walkTables(
  sessionId: Uuid,
  scope: ObjectPath,
): Promise<{ tables: CatalogTable[]; lazy: Map<CatalogTable, ObjectPath> }> {
  const tables: CatalogTable[] = [];
  const lazy = new Map<CatalogTable, ObjectPath>();
  const visit = async (path: ObjectPath, depth: number): Promise<void> => {
    if (depth > MAX_DEPTH || tables.length >= MAX_TABLES) return;
    let nodes: ObjectNode[];
    try {
      nodes = await ipc.objectChildren(sessionId, path);
    } catch {
      return;
    }
    const folders: ObjectNode[] = [];
    for (const n of nodes) {
      if (TABLE_KINDS.has(n.kind)) {
        const t: CatalogTable = {
          schema: schemaOfPath(n.path),
          name: n.name,
          kind: n.kind,
          columns: [],
        };
        tables.push(t);
        lazy.set(t, n.path);
      } else if (n.hasChildren && ['folder', 'schema', 'database'].includes(n.kind)) {
        folders.push(n);
      }
    }
    await Promise.all(folders.map((f) => visit(f.path, depth + 1)));
  };
  await visit(scope, 0);
  return { tables, lazy };
}

/** Database to complete against: the tab's, else the first one of the server. */
async function scopeFor(sessionId: Uuid, database: string | null): Promise<ObjectPath> {
  if (database) return { segments: [{ kind: 'database', name: database }] };
  try {
    const roots = await ipc.objectChildren(sessionId, ROOT_PATH);
    const first = roots.find((n) => n.kind === 'database');
    if (first) return first.path;
  } catch {
    // fall through: an empty scope asks the driver for everything
  }
  return ROOT_PATH;
}

export async function loadSessionCatalog(
  sessionId: Uuid,
  database: string | null,
  defaultSchema: string | null,
): Promise<SessionCatalog> {
  const scope = await scopeFor(sessionId, database);
  // Unsupported by the driver (or failing): fall back to walking the tree.
  const model: SchemaModel | null = await ipc
    .schemaModel(sessionId, {
      scope,
      includeViews: true,
      includeComments: false,
      detail: 'full',
    })
    .catch(() => null);
  if (model && model.tables.length > 0) {
    return new SessionCatalog(
      sessionId,
      model.tables.map(tableFromModel),
      new Map(),
      defaultSchema,
    );
  }
  const { tables, lazy } = await walkTables(sessionId, scope);
  return new SessionCatalog(sessionId, tables, lazy, defaultSchema);
}

const catalogs = new Map<string, { promise: Promise<SessionCatalog>; at: number }>();
const vocabularies = new Map<Uuid, Promise<Vocabulary>>();

/** Cached catalog of a session database (refreshed after `CACHE_MS` or `invalidateCatalog`). */
export function getSessionCatalog(
  sessionId: Uuid,
  database: string | null,
  defaultSchema: string | null,
): Promise<SessionCatalog> {
  const key = `${sessionId}\u0000${database ?? ''}\u0000${defaultSchema ?? ''}`;
  const cached = catalogs.get(key);
  if (cached && Date.now() - cached.at < CACHE_MS) return cached.promise;
  const promise = loadSessionCatalog(sessionId, database, defaultSchema);
  const entry = { promise, at: Date.now() };
  catalogs.set(key, entry);
  promise.catch(() => {
    if (catalogs.get(key) === entry) catalogs.delete(key);
  });
  return promise;
}

/** Forget cached catalogs (after DDL, or when the user refreshes the tree). */
export function invalidateCatalog(sessionId?: Uuid): void {
  for (const key of [...catalogs.keys()]) {
    if (!sessionId || key.startsWith(`${sessionId}\u0000`)) catalogs.delete(key);
  }
}

export function getVocabulary(sessionId: Uuid): Promise<Vocabulary> {
  let v = vocabularies.get(sessionId);
  if (!v) {
    v = ipc
      .scriptVocabulary(sessionId)
      .catch(() => ({ keywords: [], functions: [], commands: [] }));
    vocabularies.set(sessionId, v);
  }
  return v;
}

export function sqlVocabulary(v: Vocabulary): CompletionVocabulary {
  return { keywords: v.keywords, functions: v.functions };
}

export function resetCatalogCache(): void {
  catalogs.clear();
  vocabularies.clear();
}

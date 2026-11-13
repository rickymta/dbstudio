// Mock schema model, ERD layouts (in memory) and DDL generation.

import type {
  DdlItem,
  DdlRequest,
  ErdLayout,
  ErdLayoutSummary,
  SchemaModel,
  SchemaModelRequest,
  Uuid,
} from '../types';
import { mockSessionKind } from './sessions';
import { clone, ipcError } from './util';

let layouts: ErdLayout[] = [];

export const erdMock = {
  async schemaModel(sessionId: Uuid, request: SchemaModelRequest): Promise<SchemaModel> {
    mockSessionKind(sessionId);
    return { scope: request.scope, tables: [], relations: [], inferred: false, truncated: false };
  },
  async erdLayoutsList(connectionId: Uuid): Promise<ErdLayoutSummary[]> {
    return layouts
      .filter((l) => l.connectionId === connectionId)
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
    layouts = [...layouts.filter((l) => l.id !== layout.id), clone(layout)];
    return clone(layout);
  },
  async erdLayoutDelete(id: Uuid): Promise<void> {
    layouts = layouts.filter((l) => l.id !== id);
  },
  async ddlGenerate(sessionId: Uuid, request: DdlRequest): Promise<DdlItem[]> {
    mockSessionKind(sessionId);
    void request;
    return [];
  },
};

export function resetMockErd(): void {
  layouts = [];
}

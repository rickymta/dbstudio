import { mock } from '../mock';
import type {
  DdlItem,
  DdlRequest,
  ErdLayout,
  ErdLayoutSummary,
  SchemaModel,
  SchemaModelRequest,
  Uuid,
} from '../types';
import { call } from './core';

/** Schema model, ERD layouts and DDL generation. */
export const erdApi = {
  schemaModel: (sessionId: Uuid, request: SchemaModelRequest): Promise<SchemaModel> =>
    call('schema_model', () => mock.schemaModel(sessionId, request), { sessionId, request }),
  erdLayoutsList: (connectionId: Uuid): Promise<ErdLayoutSummary[]> =>
    call('erd_layouts_list', () => mock.erdLayoutsList(connectionId), { connectionId }),
  erdLayoutGet: (id: Uuid): Promise<ErdLayout> =>
    call('erd_layout_get', () => mock.erdLayoutGet(id), { id }),
  erdLayoutSave: (layout: ErdLayout): Promise<ErdLayout> =>
    call('erd_layout_save', () => mock.erdLayoutSave(layout), { layout }),
  erdLayoutDelete: (id: Uuid): Promise<void> =>
    call('erd_layout_delete', () => mock.erdLayoutDelete(id), { id }),
  ddlGenerate: (sessionId: Uuid, request: DdlRequest): Promise<DdlItem[]> =>
    call('ddl_generate', () => mock.ddlGenerate(sessionId, request), { sessionId, request }),
};

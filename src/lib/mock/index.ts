import { mockChildren, mockObjectDdl, mockTableDetails } from './catalog';
import * as connections from './connections';
import * as data from './data';
import { mockObjectSearch } from './search';
import * as sessions from './sessions';
import * as settings from './settings';
import { sleep } from './util';

export { mockAppInfo, mockDrivers } from './drivers';
export { resetMockConnections } from './connections';
export { resetMockSettings, mockLogDir } from './settings';

type Path = Parameters<typeof mockChildren>[1];

/** Backend stand-in used by `ipc.ts` when running outside Tauri. */
export const mock = {
  connectionsList: connections.connectionsList,
  connectionSave: connections.connectionSave,
  connectionDelete: connections.connectionDelete,
  connectionDuplicate: connections.connectionDuplicate,
  groupSave: connections.groupSave,
  groupDelete: connections.groupDelete,
  connectionTest: connections.connectionTest,
  connectionsExport: connections.connectionsExport,
  connectionsImport: connections.connectionsImport,
  sessionOpen: sessions.sessionOpen,
  sessionClose: sessions.sessionClose,
  tableFetch: data.tableFetch,
  tableCount: data.tableCount,
  queryCancel: data.queryCancel,
  setFetchMode: data.setFetchMode,
  settingsGet: settings.settingsGet,
  settingsSet: settings.settingsSet,
  saveCellValue: settings.saveCellValue,
  async objectSearch(sessionId: string, request: Parameters<typeof mockObjectSearch>[1]) {
    await sleep();
    return mockObjectSearch(sessions.mockSessionKind(sessionId), request);
  },
  async objectChildren(sessionId: string, path: Path) {
    await sleep();
    return mockChildren(sessions.mockSessionKind(sessionId), path);
  },
  async objectDdl(sessionId: string, path: Path) {
    await sleep();
    return mockObjectDdl(sessions.mockSessionKind(sessionId), path);
  },
  async tableDetails(sessionId: string, path: Path) {
    await sleep();
    return mockTableDetails(sessions.mockSessionKind(sessionId), path);
  },
};

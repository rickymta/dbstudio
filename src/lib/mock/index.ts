import { backupMock, resetMockBackup } from './backup';
import { mockChildren, mockObjectDdl, mockTableDetails } from './catalog';
import { mockParseMongoUri } from './catalogMongo';
import { changesMock } from './changes';
import { debugMock, resetMockDebug } from './debug';
import { erdMock, resetMockErd } from './erd';
import { exportMock, resetMockExport } from './export';
import { historyMock, resetMockHistory } from './history';
import { jobsMock, resetMockJobs } from './jobs';
import { resetMockRedis } from './catalogRedis';
import { keyvalueMock } from './keyvalue';
import { leasesMock, resetMockLeases } from './leases';
import { planMock, resetMockPlanHistory } from './plan';
import * as connections from './connections';
import * as data from './data';
import { mockObjectSearch } from './search';
import * as sessions from './sessions';
import * as settings from './settings';
import { sleep } from './util';
import type { DbKind, DriverDiagnostics } from '../types';
import { mockDriverFor } from './drivers';

export { mockAppInfo, mockDrivers } from './drivers';
export { resetMockConnections } from './connections';
export { resetMockSettings, mockLogDir } from './settings';
export { mockEmit, mockListen, resetMockEvents } from './events';
export { mockAddJob } from './jobs';

/** Reset every in-memory mock store (new commands; the older resets are exported above). */
export function resetMockPhase2(): void {
  resetMockLeases();
  resetMockHistory();
  resetMockErd();
  resetMockJobs();
  resetMockExport();
  resetMockBackup();
  resetMockRedis();
  resetMockPlanHistory();
  resetMockDebug();
}

type Path = Parameters<typeof mockChildren>[1];

/** Backend stand-in used by `ipc.ts` when running outside Tauri. */
export const mock = {
  ...leasesMock,
  ...historyMock,
  ...changesMock,
  ...planMock,
  ...erdMock,
  ...jobsMock,
  ...exportMock,
  ...backupMock,
  ...keyvalueMock,
  ...debugMock,
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
  sessionReconnect: sessions.sessionReconnect,
  tableFetch: data.tableFetch,
  tableCount: data.tableCount,
  queryCancel: data.queryCancel,
  setFetchMode: data.setFetchMode,
  settingsGet: settings.settingsGet,
  settingsSet: settings.settingsSet,
  saveCellValue: settings.saveCellValue,
  connectionParseUri: mockParseMongoUri,
  async driverDiagnostics(kind: DbKind): Promise<DriverDiagnostics> {
    return { status: mockDriverFor(kind).status, message: '', helpUrl: null, checks: [] };
  },
  async sshTrustHost(host: string, port: number, keyType: string, fingerprint: string) {
    void [host, port, keyType, fingerprint];
  },
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

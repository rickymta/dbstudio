import { backupApi } from './backup';
import { changesApi } from './changes';
import { coreApi } from './core';
import { debugApi } from './debug';
import { editorApi } from './editor';
import { erdApi } from './erd';
import { eventsApi } from './events';
import { exportApi } from './export';
import { filesApi } from './files';
import { jobsApi } from './jobs';
import { keyvalueApi } from './keyvalue';
import { planApi } from './plan';

export {
  ERROR_KINDS,
  STATEMENT_END_TIMEOUT_MS,
  ipcTransport,
  isTauri,
  toIpcError,
  type IpcTransport,
} from './core';
export { EVENTS, type EventPayloads } from './events';

/**
 * The single backend API of the frontend. All backend access goes through this object; each
 * area lives in its own file (core, editor, changes, plan, erd, files, jobs, export, backup,
 * keyvalue, debug, events).
 */
export const ipc = {
  ...coreApi,
  ...editorApi,
  ...changesApi,
  ...planApi,
  ...erdApi,
  ...filesApi,
  ...jobsApi,
  ...exportApi,
  ...backupApi,
  ...keyvalueApi,
  ...debugApi,
  ...eventsApi,
};

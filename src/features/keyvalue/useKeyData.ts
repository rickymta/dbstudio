import { useCallback, useEffect, useRef, useState } from 'react';
import { ipc, toIpcError } from '@/lib/ipc';
import type { IpcError, KeyEntries, KeyValuePage, ObjectPath, Uuid } from '@/lib/types';
import { mergeEntries } from './keyChanges';

/** Elements requested per `key_read` page. */
export const KEY_PAGE_SIZE = 200;

export interface KeyData {
  status: 'loading' | 'ready' | 'missing' | 'error';
  /** Metadata (type, TTL, length...) of the latest read. */
  page: KeyValuePage | null;
  /** Entries of every page loaded so far. */
  entries: KeyEntries | null;
  nextCursor: string | null;
  /** `Date.now()` of the latest read: the TTL counts down from here. */
  fetchedAt: number;
  error: IpcError | null;
  loadingMore: boolean;
  pattern: string;
}

const INITIAL: KeyData = {
  status: 'loading',
  page: null,
  entries: null,
  nextCursor: null,
  fetchedAt: 0,
  error: null,
  loadingMore: false,
  pattern: '',
};

/** Reads a key page by page; `reload` starts over (optionally with a new pattern). */
export function useKeyData(sessionId: Uuid, path: ObjectPath) {
  const [data, setData] = useState<KeyData>(INITIAL);
  const request = useRef(0);
  const dataRef = useRef(data);
  dataRef.current = data;

  const reload = useCallback(
    async (pattern?: string) => {
      const id = ++request.current;
      const pat = pattern ?? dataRef.current.pattern;
      setData((d) => ({ ...d, status: d.page ? d.status : 'loading', pattern: pat, error: null }));
      try {
        const page = await ipc.keyRead(sessionId, {
          key: path,
          cursor: null,
          count: KEY_PAGE_SIZE,
          pattern: pat || null,
        });
        if (id !== request.current) return;
        setData({
          status: 'ready',
          page,
          entries: page.entries,
          nextCursor: page.nextCursor,
          fetchedAt: Date.now(),
          error: null,
          loadingMore: false,
          pattern: pat,
        });
      } catch (e) {
        if (id !== request.current) return;
        const error = toIpcError(e);
        setData((d) => ({
          ...d,
          status: error.kind === 'notFound' ? 'missing' : 'error',
          error,
          page: error.kind === 'notFound' ? null : d.page,
          entries: error.kind === 'notFound' ? null : d.entries,
          loadingMore: false,
        }));
      }
    },
    [sessionId, path],
  );

  const loadMore = useCallback(async () => {
    const current = dataRef.current;
    if (!current.nextCursor || current.loadingMore || !current.entries) return;
    const id = request.current;
    setData((d) => ({ ...d, loadingMore: true }));
    try {
      const page = await ipc.keyRead(sessionId, {
        key: path,
        cursor: current.nextCursor,
        count: KEY_PAGE_SIZE,
        pattern: current.pattern || null,
      });
      if (id !== request.current) return;
      setData((d) => ({
        ...d,
        page,
        entries: d.entries ? mergeEntries(d.entries, page.entries) : page.entries,
        nextCursor: page.nextCursor,
        loadingMore: false,
      }));
    } catch (e) {
      if (id !== request.current) return;
      setData((d) => ({ ...d, loadingMore: false, error: toIpcError(e) }));
    }
  }, [sessionId, path]);

  useEffect(() => {
    void reload('');
    const counter = request;
    return () => {
      counter.current++;
    };
  }, [reload]);

  return { data, reload, loadMore };
}

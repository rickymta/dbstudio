import { describe, expect, it } from 'vitest';
import * as M from './model';

const ws = (over: Partial<M.Workspace> = {}): M.Workspace =>
  M.sanitize({ id: 'w', name: 'W', color: '#000', connections: ['a', 'b', 'c'], ...over });

describe('workspace folders', () => {
  it('nests at most three levels and refuses to move a folder into itself', () => {
    let w = ws();
    w = M.addFolder(w, M.ROOT, 'One', 'f1');
    w = M.addFolder(w, 'f1', 'Two', 'f2');
    w = M.addFolder(w, 'f2', 'Three', 'f3');
    expect(M.canNest(w.folders, 'f3')).toBe(false);
    expect(M.addFolder(w, 'f3', 'Four', 'f4')).toBe(w);
    expect(M.canMove(w.folders, 'f1', 'f2')).toBe(false); // into its own subtree
    w = M.addFolder(w, M.ROOT, 'Other', 'o');
    expect(M.canMove(w.folders, 'f2', 'o')).toBe(true); // f2 + f3 fit under a root folder
    expect(M.canMove(w.folders, 'f1', 'o')).toBe(false); // three levels under another: too deep
  });

  it('places connections, counts deep and moves contents up when a folder is deleted', () => {
    let w = M.addFolder(M.addFolder(ws(), M.ROOT, 'Prod', 'p'), 'p', 'EU', 'eu');
    w = M.moveConnections(w, ['a'], 'p');
    w = M.moveConnections(w, ['b'], 'eu');
    expect(M.connectionsIn(w, 'p')).toEqual(['a']);
    expect(M.connectionsIn(w, 'p', true)).toEqual(['a', 'b']);
    expect(M.connectionsIn(w, M.ROOT)).toEqual(['c']);
    w = M.deleteFolder(w, 'eu');
    expect(M.folderOf(w, 'b')).toBe('p');
    expect(w.folders.map((f) => f.id)).toEqual(['p']);
  });

  it('adds only new connections into the given folder and forgets removed ones', () => {
    let w = M.addFolder(ws(), M.ROOT, 'F', 'f');
    w = M.addConnections(w, ['b', 'd', 'd', 'e'], 'f');
    expect(w.connections).toEqual(['a', 'b', 'c', 'd', 'e']);
    expect(M.folderOf(w, 'd')).toBe('f');
    expect(M.folderOf(w, 'b')).toBe(M.ROOT); // already there: not moved
    w = M.removeConnection(w, 'd');
    expect(w.connections).not.toContain('d');
    expect(w.placement.d).toBeUndefined();
  });

  it('repairs broken data: unknown parents, cycles, too deep, stray placements', () => {
    const w = M.sanitize({
      id: 'x',
      connections: ['a', 'a', 'b'],
      folders: [
        { id: 'c1', name: 'c1', parent: 'c2', collapsed: false },
        { id: 'c2', name: 'c2', parent: 'c1', collapsed: false },
        { id: 'orphan', name: 'o', parent: 'gone', collapsed: false },
        { id: 'd1', name: 'd1', parent: '', collapsed: false },
        { id: 'd2', name: 'd2', parent: 'd1', collapsed: false },
        { id: 'd3', name: 'd3', parent: 'd2', collapsed: false },
        { id: 'd4', name: 'd4', parent: 'd3', collapsed: false },
      ],
      placement: { a: 'orphan', b: 'missing', z: 'd1' },
    });
    expect(w.connections).toEqual(['a', 'b']);
    expect(w.folders.find((f) => f.id === 'orphan')!.parent).toBe(M.ROOT);
    expect(
      w.folders.filter((f) => ['c1', 'c2'].includes(f.id)).some((f) => f.parent === M.ROOT),
    ).toBe(true);
    expect(M.depthOf(w.folders, 'd4')).toBeLessThanOrEqual(M.MAX_DEPTH);
    expect(w.placement).toEqual({ a: 'orphan' });
    expect(w.name).toBe('Workspace');
  });
});

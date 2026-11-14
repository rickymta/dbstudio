import { beforeEach, describe, expect, it } from 'vitest';
import { ipc } from '@/lib/ipc';
import { MYSQL_STAGING } from '@/features/editor/testUtils';
import type { DebugOutputEvent, RoutineRef } from '@/lib/types';
import { bp, openLease, pauseReason, recordEvents, resetDebugWorld, startDebug } from './helpers';

const OUTER: RoutineRef = { signature: 'public.f_outer(integer)' };
const INNER: RoutineRef = { signature: 'public.f_inner(integer)' };

beforeEach(resetDebugWorld);

async function vars(debugId: string, groupName: string) {
  const s = await ipc.debugSnapshot(debugId);
  const g = s.variables.find((v) => v.name === groupName)!;
  return ipc.debugVariableChildren(debugId, g.reference);
}

describe('fake routine debugging on a postgres lease', () => {
  it('support probe: available on postgres, unsupported on mysql', async () => {
    const pg = await openLease();
    expect((await ipc.debugSupport(pg)).routine).toEqual({ kind: 'available' });
    const my = await openLease(MYSQL_STAGING).catch(() => null);
    if (my) expect((await ipc.debugSupport(my)).routine.kind).toBe('unsupported');
  });

  it('step into f_outer, into f_inner (2 + script frames), deposit, step out, resume', async () => {
    const leaseId = await openLease();
    const events = await recordEvents();
    const s = await startDebug(leaseId, 'select f_outer(1);\nselect 2;');
    expect(s.stepIntoTargets).toEqual([OUTER]);

    const inOuter = await ipc.debugStep(s.debugId, { kind: 'stepInto' });
    expect(pauseReason(inOuter)).toBe('routineEntry');
    expect(inOuter.frames.map((f) => f.name)).toEqual(['f_outer', 'script.sql:1']);
    expect(inOuter.position).toMatchObject({
      source: { kind: 'routine', routine: OUTER },
      line: 3,
      step: 0,
    });

    const src = await ipc.debugRoutineSource(s.debugId, 'x', OUTER);
    expect(src.text.split('\n')[2]).toContain('p_n * 2');

    // line 3 executes: v_total = 2
    const l4 = await ipc.debugStep(s.debugId, { kind: 'stepOver' });
    expect(l4.position?.line).toBe(4);
    const locals = await vars(s.debugId, 'Routine: f_outer');
    expect(locals.map((v) => [v.name, v.display, v.kind])).toEqual([
      ['p_n', '1', 'routineArgument'],
      ['v_total', '2', 'routineLocal'],
    ]);
    expect(locals[1]!.changed).toBe(true);
    expect(l4.inlineValues).toContainEqual(
      expect.objectContaining({
        line: 3,
        text: 'v_total = 2',
        source: { kind: 'routine', routine: OUTER },
      }),
    );

    // nested step into
    const inInner = await ipc.debugStep(s.debugId, { kind: 'stepInto' });
    expect(pauseReason(inInner)).toBe('routineEntry');
    expect(inInner.frames.map((f) => f.name)).toEqual(['f_inner', 'f_outer', 'script.sql:1']);
    expect(inInner.frames[0]!.args).toBe('p_x=2');
    const innerVars = await vars(s.debugId, 'Routine: f_inner');
    const px = innerVars.find((v) => v.name === 'p_x')!;
    expect(px.editable).toBe(true);

    // deposit a new value into the argument; selecting the outer frame shows its group
    const set = await ipc.debugSetVariable(s.debugId, px.reference, '10');
    expect(set.display).toBe('10');
    const outerFrame = await ipc.debugSelectFrame(s.debugId, 1);
    expect(outerFrame.variables.find((v) => v.name.startsWith('Routine:'))?.name).toBe(
      'Routine: f_outer',
    );
    await ipc.debugSelectFrame(s.debugId, 0);
    await expect(ipc.debugSetVariable(s.debugId, px.reference, 'abc')).rejects.toMatchObject({
      kind: 'invalid',
    });

    // step out of f_inner: f_outer line 5 with v_total = 2 + (10 + 1)
    const out = await ipc.debugStep(s.debugId, { kind: 'stepOut' });
    expect(pauseReason(out)).toBe('step');
    expect(out.frames.map((f) => f.name)).toEqual(['f_outer', 'script.sql:1']);
    expect(out.position?.line).toBe(5);
    const after = await vars(s.debugId, 'Routine: f_outer');
    expect(after.find((v) => v.name === 'v_total')?.display).toBe('13');

    // step out of f_outer: back in the script on the next statement
    const back = await ipc.debugStep(s.debugId, { kind: 'stepOut' });
    expect(back.frames).toHaveLength(1);
    expect(back.position).toMatchObject({ source: { kind: 'script' }, line: 2, step: 1 });
    await expect(ipc.debugStep(s.debugId, { kind: 'stepOut' })).rejects.toMatchObject({
      kind: 'invalid',
    });

    const rowEvents = events.outputs.filter(
      (e): e is DebugOutputEvent & { output: { kind: 'query' } } => e.output.kind === 'query',
    );
    const rows = rowEvents.flatMap((e) =>
      e.output.event.type === 'rows' ? e.output.event.rows : [],
    );
    expect(rows[0]).toEqual([{ t: 'int', v: 13 }]);
    events.stop();
    await ipc.debugResume(s.debugId);
    await ipc.debugStop(s.debugId, 'rollback');
  });

  it('step over a routine call stops inside only when a routine breakpoint is set', async () => {
    const leaseId = await openLease();
    const s = await startDebug(leaseId, 'select f_outer(3);\nselect 2;');
    const over = await ipc.debugStep(s.debugId, { kind: 'stepOver' });
    expect(over.position).toMatchObject({ source: { kind: 'script' }, line: 2 });

    const withBp = await startDebug(await openLease(), 'select f_outer(3);\nselect 2;', {
      routineBreakpoints: [{ routine: INNER, breakpoint: bp(3) }],
    });
    // routine breakpoint -> no stop on entry, runs until it hits inside f_inner
    expect(pauseReason(withBp)).toBe('breakpoint');
    expect(withBp.frames.map((f) => f.name)).toEqual(['f_inner', 'f_outer', 'script.sql:1']);
  });

  it('routine breakpoints: condition, hit count, stale source hash', async () => {
    const leaseId = await openLease();
    const miss = bp(4, { condition: 'v_total = 99' });
    const hit = bp(4, { condition: 'v_total = 6' });
    const s = await startDebug(leaseId, 'select f_outer(3);', {
      routineBreakpoints: [
        { routine: OUTER, breakpoint: miss },
        { routine: OUTER, breakpoint: hit },
      ],
    });
    expect(s.status).toMatchObject({ reason: { kind: 'breakpoint', ids: [hit.id] } });
    expect(s.position).toMatchObject({ line: 4 });
    await ipc.debugStop(s.debugId, 'rollback');

    const second = await openLease();
    const nth = bp(3, { hitCount: 2 });
    const t = await startDebug(second, 'select f_inner(1);\nselect f_inner(2);', {
      routineBreakpoints: [{ routine: INNER, breakpoint: nth }],
    });
    expect(t.frames[0]!.args).toBe('p_x=2'); // stops on the second call only

    const stale = await startDebug(await openLease(), 'select f_inner(1);', {
      routineBreakpoints: [{ routine: INNER, sourceHash: 'deadbeef', breakpoint: bp(3) }],
    });
    expect(stale.breakpoints[0]).toMatchObject({ verified: false, message: 'source has changed' });
    // an unverified breakpoint never stops: the script runs to its end
    expect(pauseReason(stale)).toBe('awaitingEnd');
  });

  it('evaluate and watches are read-only and frame aware', async () => {
    const leaseId = await openLease();
    const s = await startDebug(leaseId, 'select f_outer(4);');
    await ipc.debugWatchesSet(s.debugId, [{ id: 'w1', expr: 'v_total + 1' }]);
    await ipc.debugStep(s.debugId, { kind: 'stepInto' });
    const results = await ipc.debugWatchesSet(s.debugId, [
      { id: 'w1', expr: 'v_total + 1' },
      { id: 'w2', expr: 'p_n * 10' },
      { id: 'w3', expr: 'select * from users' },
      { id: 'w4', expr: 'drop table users' },
      { id: 'w5', expr: 'missing_var' },
    ]);
    expect(results[0]!.result.value.kind).toBe('error'); // v_total not assigned yet
    expect(results[1]!.result).toMatchObject({
      context: 'proxy',
      value: { kind: 'scalar', value: { t: 'int', v: 40 } },
    });
    expect(results[2]!.result.value.kind).toBe('rows');
    expect(results[3]!.result.value).toMatchObject({ kind: 'error', error: { kind: 'readOnly' } });
    expect(results[4]!.result.value.kind).toBe('error');
    const bare = await ipc.debugEvaluate(s.debugId, 'p_n');
    expect(bare).toMatchObject({ context: 'routineVariable', value: { kind: 'scalar' } });
    // the script frame has no routine variables
    const scriptFrame = await ipc.debugEvaluate(s.debugId, 'p_n', 1);
    expect(scriptFrame.value.kind).toBe('error');

    // watches follow the session: re-evaluated on every pause
    const next = await ipc.debugStep(s.debugId, { kind: 'stepOver' });
    expect(next.watches[0]!.result.value).toMatchObject({
      kind: 'scalar',
      value: { t: 'int', v: 9 },
    });
  });
});

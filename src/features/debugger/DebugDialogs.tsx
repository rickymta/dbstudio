import { useState, type FC } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Checkbox } from '@/components/ui/Checkbox';
import { Dialog, DialogContent } from '@/components/ui/Dialog';
import { Textarea } from '@/components/ui/Textarea';
import { toast } from '@/components/ui/toastStore';
import type { Breakpoint, EvalResult } from '@/lib/types';
import {
  addWatch,
  answerEnd,
  editorConnectionId,
  evaluateExpression,
  historyOf,
  insertIntoScript,
  pushHistory,
  removeAllBreakpoints,
  removeBreakpoint,
  setMuted,
  step,
  supportOf,
  updateBreakpoint,
} from './debugActions';
import { isLive, scriptScope, useDebugStore } from './debugStore';
import { useDebugUi } from './debugUiStore';
import { BreakpointForm } from './BreakpointForm';
import { EvalView } from './ConsolePanel';

const EMPTY: never[] = [];

function BreakpointsDialog({ tabId }: { tabId: string }) {
  const { t } = useTranslation('debugger');
  const close = () => useDebugUi.getState().closeDialog(tabId);
  const scopes = useDebugStore((s) => s.scopes);
  const muted = useDebugStore((s) => s.muted[tabId] ?? false);
  const connectionId = editorConnectionId(tabId);
  const keys = Object.keys(scopes).filter(
    (k) =>
      k === scriptScope(tabId) ||
      (connectionId !== null && scopes[k]!.connectionId === connectionId),
  );
  const [selected, setSelected] = useState<{ key: string; id: string } | null>(null);
  const current = selected
    ? scopes[selected.key]?.breakpoints.find((b) => b.id === selected.id)
    : undefined;
  const total = keys.reduce((n, k) => n + scopes[k]!.breakpoints.length, 0);

  const label = (b: Breakpoint) =>
    `${t('breakpoint.line', { line: b.line })}${b.condition ? ` · ${b.condition}` : ''}`;

  return (
    <Dialog open onOpenChange={(open) => !open && close()}>
      <DialogContent
        title={t('breakpoints.title')}
        className="w-[min(760px,94vw)]"
        footer={
          <>
            <Button
              disabled={total === 0}
              onClick={() => {
                removeAllBreakpoints(keys);
                setSelected(null);
              }}
            >
              {t('breakpoints.removeAll')}
            </Button>
            <Button aria-pressed={muted} onClick={() => void setMuted(tabId, !muted)}>
              {muted ? t('breakpoints.unmute') : t('breakpoints.mute')}
            </Button>
            <Button variant="primary" onClick={close}>
              {t('actions.close')}
            </Button>
          </>
        }
      >
        <div className="flex min-h-64 gap-4">
          <div
            className="w-1/2 min-w-0 overflow-auto"
            role="tree"
            aria-label={t('breakpoints.title')}
          >
            {total === 0 && (
              <p className="text-xs text-muted-foreground">{t('breakpoints.empty')}</p>
            )}
            {keys.map((key) => {
              const scope = scopes[key]!;
              if (scope.breakpoints.length === 0) return null;
              const heading = scope.routine
                ? t('breakpoints.groupRoutine', { name: scope.routine.signature })
                : t('breakpoints.groupScript');
              return (
                <div key={key} role="group" aria-label={heading} className="mb-2">
                  <div className="text-xs font-medium text-muted-foreground">{heading}</div>
                  {scope.breakpoints.map((b) => (
                    <div
                      key={b.id}
                      role="treeitem"
                      aria-selected={selected?.id === b.id}
                      tabIndex={0}
                      className={`flex cursor-default items-center gap-2 rounded px-1 py-0.5 text-[13px] ${selected?.id === b.id ? 'bg-accent/15' : 'hover:bg-surface-muted'}`}
                      onClick={() => setSelected({ key, id: b.id })}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter') setSelected({ key, id: b.id });
                        if (e.key === 'Delete') removeBreakpoint(key, b.id);
                      }}
                    >
                      <Checkbox
                        aria-label={t('breakpoints.toggle', { line: b.line })}
                        checked={b.enabled}
                        onClick={(e) => e.stopPropagation()}
                        onCheckedChange={(v) =>
                          updateBreakpoint(key, b.id, { enabled: v === true })
                        }
                      />
                      <span className="truncate font-mono">{label(b)}</span>
                    </div>
                  ))}
                </div>
              );
            })}
          </div>
          <div className="w-1/2 min-w-0 border-l border-border pl-4">
            {selected && current ? (
              <>
                <BreakpointForm key={current.id} scopeKey={selected.key} breakpoint={current} />
                <Button
                  className="mt-3"
                  size="sm"
                  onClick={() => {
                    removeBreakpoint(selected.key, current.id);
                    setSelected(null);
                  }}
                >
                  {t('breakpoint.remove')}
                </Button>
              </>
            ) : (
              <p className="text-xs text-muted-foreground">{t('breakpoints.select')}</p>
            )}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function EvaluateDialog({ tabId, initial }: { tabId: string; initial: string }) {
  const { t } = useTranslation('debugger');
  const close = () => useDebugUi.getState().closeDialog(tabId);
  const live = useDebugStore((s) => isLive(s.sessions[tabId]));
  const [expr, setExpr] = useState(initial);
  const [result, setResult] = useState<EvalResult | null>(null);
  const [busy, setBusy] = useState(false);
  const history = historyOf(tabId);

  const run = async () => {
    const text = expr.trim();
    if (!text || !live) return;
    setBusy(true);
    pushHistory(tabId, text);
    setResult(await evaluateExpression(tabId, text));
    setBusy(false);
  };

  return (
    <Dialog open onOpenChange={(open) => !open && close()}>
      <DialogContent
        title={t('evaluate.title')}
        className="w-[min(640px,94vw)]"
        footer={
          <>
            <Button disabled={!expr.trim()} onClick={() => void addWatch(tabId, expr)}>
              {t('evaluate.addWatch')}
            </Button>
            <Button
              variant="primary"
              disabled={!live || busy || !expr.trim()}
              onClick={() => void run()}
            >
              {t('evaluate.evaluate')}
            </Button>
          </>
        }
      >
        <div className="flex flex-col gap-2">
          <Textarea
            autoFocus
            rows={3}
            value={expr}
            aria-label={t('evaluate.expression')}
            placeholder={t('evaluate.placeholder')}
            onChange={(e) => setExpr(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
                e.preventDefault();
                void run();
              }
            }}
          />
          {history.length > 0 && (
            <div className="flex flex-wrap gap-1" aria-label={t('evaluate.history')}>
              {[...history]
                .reverse()
                .slice(0, 8)
                .map((h) => (
                  <button
                    key={h}
                    type="button"
                    className="max-w-48 truncate rounded border border-border px-1.5 py-px font-mono text-xs hover:bg-surface-muted"
                    onClick={() => setExpr(h)}
                  >
                    {h}
                  </button>
                ))}
            </div>
          )}
          <div aria-live="polite" data-testid="evaluate-result">
            {result && <EvalView result={result} />}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function EndDialog({ tabId }: { tabId: string }) {
  const { t } = useTranslation('debugger');
  const close = () => useDebugUi.getState().closeDialog(tabId);
  return (
    <Dialog open onOpenChange={(open) => !open && close()}>
      <DialogContent
        title={t('end.title')}
        description={t('end.description')}
        footer={
          <>
            <Button onClick={close}>{t('actions.cancel')}</Button>
            <Button onClick={() => answerEnd(tabId, 'keep')}>{t('end.keep')}</Button>
            <Button variant="danger" onClick={() => answerEnd(tabId, 'commit')}>
              {t('end.commit')}
            </Button>
            <Button variant="primary" autoFocus onClick={() => answerEnd(tabId, 'rollback')}>
              {t('end.rollback')}
            </Button>
          </>
        }
      />
    </Dialog>
  );
}

function SmartStepDialog({ tabId }: { tabId: string }) {
  const { t } = useTranslation('debugger');
  const close = () => useDebugUi.getState().closeDialog(tabId);
  const targets = useDebugStore((s) => s.sessions[tabId]?.state.stepIntoTargets ?? EMPTY);
  return (
    <Dialog open onOpenChange={(open) => !open && close()}>
      <DialogContent title={t('smart.title')} description={t('smart.description')}>
        <div className="flex flex-col gap-1">
          {targets.map((target) => (
            <Button
              key={target.signature}
              className="justify-start font-mono"
              onClick={() => {
                close();
                void step(tabId, { kind: 'smartStepInto', signature: target.signature });
              }}
            >
              {target.signature}
            </Button>
          ))}
        </div>
      </DialogContent>
    </Dialog>
  );
}

function RoutineSupportDialog({ tabId }: { tabId: string }) {
  const { t } = useTranslation('debugger');
  const close = () => useDebugUi.getState().closeDialog(tabId);
  const routine = supportOf(tabId)?.routine;
  let body = t('support.unknown');
  if (routine) {
    switch (routine.kind) {
      case 'available':
        body = t('support.available');
        break;
      case 'needsExtension':
        body = t('support.needsExtension');
        break;
      case 'needsServerConfig':
        body = `${t('support.needsServerConfig')} ${routine.hint}`;
        break;
      case 'noPermission':
        body = `${t('support.noPermission')} ${routine.hint}`;
        break;
      case 'unsupported':
        body = `${t('support.unsupported')} ${routine.reason}`;
        break;
    }
  }
  return (
    <Dialog open onOpenChange={(open) => !open && close()}>
      <DialogContent
        title={t('support.title')}
        footer={
          <>
            {routine?.kind === 'needsExtension' && (
              <Button
                variant="primary"
                onClick={() => {
                  insertIntoScript(tabId, routine.statement);
                  close();
                }}
              >
                {t('support.insert')}
              </Button>
            )}
            <Button onClick={close}>{t('actions.close')}</Button>
          </>
        }
      >
        <p className="text-[13px]">{body}</p>
        {routine?.kind === 'needsExtension' && (
          <pre className="mt-2 rounded bg-surface-muted p-2 font-mono text-xs">
            {routine.statement}
          </pre>
        )}
      </DialogContent>
    </Dialog>
  );
}

function InspectDialog({ tabId, name, text }: { tabId: string; name: string; text: string }) {
  const { t } = useTranslation('debugger');
  const close = () => useDebugUi.getState().closeDialog(tabId);
  return (
    <Dialog open onOpenChange={(open) => !open && close()}>
      <DialogContent
        title={t('inspect.title', { name })}
        className="w-[min(640px,94vw)]"
        footer={
          <>
            <Button
              onClick={() =>
                void navigator.clipboard
                  ?.writeText(text)
                  .catch(() => toast.error(t('grid.copyFailed')))
              }
            >
              {t('variables.menu.copyValue')}
            </Button>
            <Button onClick={close}>{t('actions.close')}</Button>
          </>
        }
      >
        <pre className="max-h-96 overflow-auto font-mono text-xs whitespace-pre-wrap">{text}</pre>
      </DialogContent>
    </Dialog>
  );
}

/** Renders the dialog requested for the tab (Breakpoints, Evaluate, end of transaction...). */
export const DebugDialogs: FC<{ tabId: string }> = ({ tabId }) => {
  const dialog = useDebugUi((s) => s.dialogs[tabId]);
  if (!dialog) return null;
  switch (dialog.kind) {
    case 'breakpoints':
      return <BreakpointsDialog tabId={tabId} />;
    case 'evaluate':
      return <EvaluateDialog tabId={tabId} initial={dialog.expr ?? ''} />;
    case 'end':
      return <EndDialog tabId={tabId} />;
    case 'smartStepInto':
      return <SmartStepDialog tabId={tabId} />;
    case 'routineSupport':
      return <RoutineSupportDialog tabId={tabId} />;
    case 'inspect':
      return <InspectDialog tabId={tabId} name={dialog.name} text={dialog.text} />;
  }
};

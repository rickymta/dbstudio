// Human readable status of a debug session for the tool window header and the live region.

import type { TFunction } from 'i18next';
import type { DebugState, PauseReason } from '@/lib/types';

function pauseText(t: TFunction, reason: PauseReason): string {
  switch (reason.kind) {
    case 'breakpoint':
      return t('debugger:status.pause.breakpoint');
    case 'error':
      return t('debugger:status.pause.error', { message: reason.error.message });
    case 'blocked':
      return t('debugger:status.pause.blocked', { message: reason.error.message });
    default:
      return t(`debugger:status.pause.${reason.kind}`);
  }
}

/** One line: "Paused at line 8 (statement 3/12), breakpoint". */
export function statusText(t: TFunction, state: DebugState): string {
  const s = state.status;
  switch (s.kind) {
    case 'starting':
      return t('debugger:status.starting');
    case 'running':
      return t('debugger:status.running', { step: s.step });
    case 'evaluating':
      return t('debugger:status.evaluating');
    case 'terminated':
      return t(`debugger:status.terminated.${s.reason}`);
    case 'paused': {
      const pos = state.position;
      const where = pos
        ? t('debugger:status.at', { line: pos.line, step: pos.step + 1, total: pos.stepCount })
        : '';
      return `${t('debugger:status.paused')} ${where}, ${pauseText(t, s.reason)}`
        .replace(/\s+,/, ',')
        .trim();
    }
  }
}

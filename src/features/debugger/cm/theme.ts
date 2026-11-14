// Look of the debugger inside the editor; colors come from the design tokens so light and dark
// follow the app theme.

import { EditorView } from '@codemirror/view';

export const debuggerTheme = EditorView.baseTheme({
  '.cm-bp-gutter': { width: '18px', cursor: 'pointer' },
  '.cm-bp-gutter .cm-gutterElement': {
    display: 'flex',
    alignItems: 'center',
    justifyContent: 'center',
    padding: '0',
  },
  '.cm-bp-gutter .cm-gutterElement:hover::before': {
    content: '""',
    width: '10px',
    height: '10px',
    borderRadius: '50%',
    backgroundColor: 'color-mix(in srgb, var(--danger) 35%, transparent)',
    position: 'absolute',
  },
  '.cm-bp': {
    display: 'inline-block',
    width: '11px',
    height: '11px',
    boxSizing: 'border-box',
    borderRadius: '50%',
    backgroundColor: 'var(--danger)',
  },
  '.cm-bp-spacer': { display: 'inline-block', width: '11px', height: '11px' },
  '.cm-bp-cond': {
    borderRadius: '2px',
    transform: 'rotate(45deg) scale(0.85)',
  },
  '.cm-bp-off': {
    backgroundColor: 'transparent',
    border: '2px solid var(--danger)',
    opacity: '0.6',
  },
  '.cm-bp-log': { backgroundColor: 'var(--warning)' },
  '.cm-bp-unverified': {
    backgroundColor: 'transparent',
    border: '2px solid var(--muted-foreground)',
  },
  '.cm-exec-gutter': { width: '16px' },
  '.cm-exec-arrow': { color: 'var(--accent)', fontSize: '12px', lineHeight: '1' },
  '.cm-exec-arrow-call': { opacity: '0.55' },
  '.cm-exec-arrow-error': { color: 'var(--danger)' },
  '.cm-exec-line-current': {
    backgroundColor: 'color-mix(in srgb, var(--accent) 22%, transparent)',
  },
  '.cm-exec-line-call': {
    backgroundColor: 'color-mix(in srgb, var(--accent) 9%, transparent)',
  },
  '.cm-exec-line-error': {
    backgroundColor: 'color-mix(in srgb, var(--danger) 26%, transparent)',
  },
  '.cm-inline-value': {
    marginLeft: '2em',
    color: 'var(--muted-foreground)',
    fontStyle: 'italic',
    opacity: '0.85',
    userSelect: 'none',
  },
  '.cm-inline-value-error': { color: 'var(--danger)' },
  '.cm-debug-hover': {
    padding: '4px 8px',
    maxWidth: '420px',
    fontFamily: 'var(--data-font-family)',
    fontSize: '12px',
    whiteSpace: 'pre-wrap',
    wordBreak: 'break-word',
  },
  '.cm-debug-hover-type': { color: 'var(--muted-foreground)', marginLeft: '6px' },
});

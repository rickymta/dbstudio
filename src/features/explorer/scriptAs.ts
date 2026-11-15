import i18n from '@/lib/i18n';
import { toast } from '@/components/ui/toastStore';
import { openEditor, uniqueEditorTitle } from '@/features/editor/open';
import { describeError } from '@/features/connections/errors';
import { ipc } from '@/lib/ipc';
import type { ObjectNode, ScriptTemplate, SessionInfo } from '@/lib/types';

/** `SELECT orders` */
export function scriptTitle(template: ScriptTemplate, node: ObjectNode): string {
  return `${template.toUpperCase()} ${node.name}`;
}

/** Open a new editor on the node's connection / database / schema holding the template. Nothing is run. */
export async function openScriptInEditor(
  session: SessionInfo,
  node: ObjectNode,
  template: ScriptTemplate,
): Promise<void> {
  try {
    const script = await ipc.scriptGenerate(session.sessionId, node.path, template);
    openEditor(session, {
      path: node.path,
      text: script.text,
      title: uniqueEditorTitle(scriptTitle(template, node)),
    });
  } catch (e) {
    toast.error(describeError(i18n.t.bind(i18n), e));
  }
}

/** Copy the template of the node to the clipboard. */
export async function copyScript(
  session: SessionInfo,
  node: ObjectNode,
  template: ScriptTemplate,
): Promise<void> {
  try {
    const script = await ipc.scriptGenerate(session.sessionId, node.path, template);
    await navigator.clipboard.writeText(script.text);
    toast.info(i18n.t('explorer:copied'));
  } catch (e) {
    toast.error(describeError(i18n.t.bind(i18n), e));
  }
}

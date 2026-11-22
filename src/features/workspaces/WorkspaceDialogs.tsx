import { Check } from 'lucide-react';
import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Checkbox } from '@/components/ui/Checkbox';
import { Dialog, DialogClose, DialogContent } from '@/components/ui/Dialog';
import { Input } from '@/components/ui/Input';
import { Radio, RadioGroup } from '@/components/ui/RadioGroup';
import { toast } from '@/components/ui/toastStore';
import { groupPathOptions } from '@/features/connections/connectionsTree';
import { kindBadgeFor } from '@/features/connections/kindConfig';
import { useConnectionsSnapshot } from '@/features/connections/queries';
import { cn } from '@/lib/cn';
import type { Uuid } from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { useWorkspaceDialogs } from './dialogs';
import { addConnections, WORKSPACE_COLORS } from './model';
import { useWorkspaces } from './store';

/** The eight workspace colours; the chosen one has a ring and a tick. */
export function ColorSwatches({
  value,
  onChange,
}: {
  value: string;
  onChange: (c: string) => void;
}) {
  const { t } = useTranslation('workspaces');
  return (
    <div className="flex flex-wrap gap-2" role="radiogroup" aria-label={t('color')}>
      {WORKSPACE_COLORS.map((c) => (
        <button
          key={c}
          type="button"
          role="radio"
          aria-checked={value === c}
          aria-label={c}
          onClick={() => onChange(c)}
          className={cn(
            'flex size-6 items-center justify-center rounded-full outline-none',
            'focus-visible:ring-2 focus-visible:ring-accent/60',
            value === c && 'ring-2 ring-foreground/70 ring-offset-2 ring-offset-surface',
          )}
          style={{ backgroundColor: c }}
        >
          {value === c && <Check size={13} strokeWidth={3} className="text-black/70" />}
        </button>
      ))}
    </div>
  );
}

/**
 * Checklist of saved connections (with their group path), filterable by name or host. Ids in
 * `already` are shown ticked and disabled ("already added").
 */
function ConnectionChecklist({
  selected,
  onChange,
  already = [],
}: {
  selected: Set<Uuid>;
  onChange: (next: Set<Uuid>) => void;
  already?: Uuid[];
}) {
  const { t } = useTranslation('workspaces');
  const { data: snapshot } = useConnectionsSnapshot();
  const [filter, setFilter] = useState('');
  const groupLabels = useMemo(
    () => new Map(snapshot ? groupPathOptions(snapshot).map((g) => [g.id, g.label]) : []),
    [snapshot],
  );
  const done = new Set(already);
  const needle = filter.trim().toLowerCase();
  const profiles = (snapshot?.profiles ?? [])
    .map((p) => ({ p, group: p.groupId ? (groupLabels.get(p.groupId) ?? '') : '' }))
    .filter(
      ({ p, group }) => !needle || `${p.name} ${p.host} ${group}`.toLowerCase().includes(needle),
    )
    .sort((a, b) => a.group.localeCompare(b.group) || a.p.sortOrder - b.p.sortOrder);
  const pickable = profiles.filter(({ p }) => !done.has(p.id)).map(({ p }) => p.id);
  const toggle = (id: Uuid, on: boolean) => {
    const next = new Set(selected);
    if (on) next.add(id);
    else next.delete(id);
    onChange(next);
  };
  return (
    <div className="space-y-2">
      <div className="flex items-center gap-2">
        <Input
          type="search"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder={t('picker.filter')}
          aria-label={t('picker.filter')}
          className="h-7 flex-1 text-xs"
        />
        <Button
          size="sm"
          variant="ghost"
          onClick={() => onChange(new Set([...selected, ...pickable]))}
        >
          {t('picker.all')}
        </Button>
        <Button
          size="sm"
          variant="ghost"
          onClick={() => onChange(new Set([...selected].filter((id) => !pickable.includes(id))))}
        >
          {t('picker.none')}
        </Button>
      </div>
      <ul
        className="max-h-64 overflow-y-auto rounded-md border border-border p-1"
        data-testid="workspace-connection-picker"
      >
        {profiles.length === 0 && (
          <li className="px-2 py-3 text-center text-xs text-muted-foreground">
            {t('picker.empty')}
          </li>
        )}
        {profiles.map(({ p, group }) => {
          const isDone = done.has(p.id);
          return (
            <li key={p.id}>
              <label
                className={cn(
                  'flex h-8 items-center gap-2 rounded px-2 text-[13px] hover:bg-surface-muted',
                  isDone && 'opacity-60',
                )}
              >
                <Checkbox
                  checked={isDone || selected.has(p.id)}
                  disabled={isDone}
                  onCheckedChange={(v) => toggle(p.id, v === true)}
                />
                <span className="w-[22px] shrink-0 rounded bg-surface-muted text-center text-[9px] font-semibold leading-4 text-muted-foreground">
                  {kindBadgeFor(p.kind)}
                </span>
                <span className="min-w-0 truncate">{p.name}</span>
                <span className="ml-auto max-w-[45%] shrink-0 truncate text-xs text-muted-foreground">
                  {isDone ? t('picker.already') : group || p.host}
                </span>
              </label>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

type Source = 'pick' | 'open' | 'empty';

function NewWorkspaceDialog({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation('workspaces');
  const count = useWorkspaces((s) => s.items.length);
  const create = useWorkspaces((s) => s.create);
  // Select the record (stable), not a fresh key array: a new array per read loops forever.
  const sessionsByConnection = useSessionsStore((s) => s.sessionsByConnection);
  const openIds = useMemo(() => Object.keys(sessionsByConnection), [sessionsByConnection]);
  const [name, setName] = useState('');
  const [color, setColor] = useState<string>(WORKSPACE_COLORS[count % WORKSPACE_COLORS.length]!);
  const [source, setSource] = useState<Source>(openIds.length > 0 ? 'open' : 'pick');
  const [picked, setPicked] = useState<Set<Uuid>>(() => new Set());
  const chosen = source === 'pick' ? [...picked] : source === 'open' ? openIds : [];
  const submit = () => {
    if (!name.trim()) return;
    const ws = create(name, color, chosen);
    toast.success(t('toast.created', { name: ws.name }));
    onClose();
  };
  return (
    <DialogContent
      title={t('new.title')}
      description={t('new.description')}
      className="w-[min(560px,94vw)]"
      footer={
        <>
          <span className="mr-auto self-center text-xs text-muted-foreground">
            {t('new.selected', { count: chosen.length })}
          </span>
          <DialogClose asChild>
            <Button>{t('common:actions.cancel')}</Button>
          </DialogClose>
          <Button variant="primary" disabled={!name.trim()} onClick={submit}>
            {t('new.create')}
          </Button>
        </>
      }
    >
      <form
        className="space-y-4"
        onSubmit={(e) => {
          e.preventDefault();
          submit();
        }}
      >
        <label className="block space-y-1">
          <span className="text-xs font-medium text-muted-foreground">{t('name')}</span>
          <Input
            autoFocus
            value={name}
            maxLength={60}
            onChange={(e) => setName(e.target.value)}
            placeholder={t('new.namePlaceholder')}
          />
        </label>
        <div className="space-y-1.5">
          <span className="text-xs font-medium text-muted-foreground">{t('color')}</span>
          <ColorSwatches value={color} onChange={setColor} />
        </div>
        <fieldset className="space-y-2">
          <legend className="mb-1 text-xs font-medium text-muted-foreground">
            {t('new.source')}
          </legend>
          <RadioGroup value={source} onValueChange={(v) => setSource(v as Source)}>
            <label className="flex items-center gap-2 text-[13px]">
              <Radio value="pick" />
              {t('new.sourcePick')}
            </label>
            <label className="flex items-center gap-2 text-[13px]">
              <Radio value="open" disabled={openIds.length === 0} />
              {t('new.sourceOpen', { count: openIds.length })}
            </label>
            <label className="flex items-center gap-2 text-[13px]">
              <Radio value="empty" />
              {t('new.sourceEmpty')}
            </label>
          </RadioGroup>
          {source === 'pick' && <ConnectionChecklist selected={picked} onChange={setPicked} />}
        </fieldset>
      </form>
    </DialogContent>
  );
}

function AddConnectionsDialog({
  workspaceId,
  folder,
  onClose,
}: {
  workspaceId: string;
  folder: string;
  onClose: () => void;
}) {
  const { t } = useTranslation('workspaces');
  const ws = useWorkspaces((s) => s.items.find((w) => w.id === workspaceId));
  const edit = useWorkspaces((s) => s.edit);
  const [picked, setPicked] = useState<Set<Uuid>>(() => new Set());
  if (!ws) return null;
  const submit = () => {
    edit(ws.id, (w) => addConnections(w, [...picked], folder));
    toast.success(t('toast.added', { count: picked.size, name: ws.name }));
    onClose();
  };
  return (
    <DialogContent
      title={t('add.title', { name: ws.name })}
      description={t('add.description')}
      className="w-[min(560px,94vw)]"
      footer={
        <>
          <DialogClose asChild>
            <Button>{t('common:actions.cancel')}</Button>
          </DialogClose>
          <Button variant="primary" disabled={picked.size === 0} onClick={submit}>
            {t('add.submit', { count: picked.size })}
          </Button>
        </>
      }
    >
      <ConnectionChecklist selected={picked} onChange={setPicked} already={ws.connections} />
    </DialogContent>
  );
}

function NameDialog({
  title,
  label,
  initial,
  submitLabel,
  onSubmit,
  onClose,
}: {
  title: string;
  label: string;
  initial: string;
  submitLabel: string;
  onSubmit: (name: string) => void;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const [name, setName] = useState(initial);
  const submit = () => {
    if (!name.trim()) return;
    onSubmit(name.trim());
    onClose();
  };
  return (
    <DialogContent
      title={title}
      className="w-[min(420px,92vw)]"
      footer={
        <>
          <DialogClose asChild>
            <Button>{t('common:actions.cancel')}</Button>
          </DialogClose>
          <Button variant="primary" disabled={!name.trim()} onClick={submit}>
            {submitLabel}
          </Button>
        </>
      }
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          submit();
        }}
      >
        <label className="block space-y-1">
          <span className="text-xs font-medium text-muted-foreground">{label}</span>
          <Input autoFocus value={name} maxLength={60} onChange={(e) => setName(e.target.value)} />
        </label>
      </form>
    </DialogContent>
  );
}

/** Renders the open workspace dialog. Mounted once in the app. */
export function WorkspaceDialogsHost() {
  const dialog = useWorkspaceDialogs((s) => s.dialog);
  const close = useWorkspaceDialogs((s) => s.close);
  return (
    <Dialog open={dialog !== null} onOpenChange={(open) => !open && close()}>
      {dialog?.type === 'new' && <NewWorkspaceDialog onClose={close} />}
      {dialog?.type === 'add' && (
        <AddConnectionsDialog
          workspaceId={dialog.workspaceId}
          folder={dialog.folder}
          onClose={close}
        />
      )}
      {dialog?.type === 'name' && <NameDialog {...dialog} onClose={close} />}
    </Dialog>
  );
}

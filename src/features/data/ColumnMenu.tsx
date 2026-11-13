import { Columns3, Pin, PinOff } from 'lucide-react';
import { Popover } from 'radix-ui';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Checkbox } from '@/components/ui/Checkbox';
import { IconButton } from '@/components/ui/IconButton';
import { Input } from '@/components/ui/Input';
import { menuContentClass } from '@/components/ui/menuStyles';
import type { ColumnMeta } from '@/lib/types';
import {
  isDefaultLayout,
  reconcileLayout,
  resetLayout,
  setPinned,
  showAll,
  toggleHidden,
  type ColumnLayout,
} from './columnLayout';

interface ColumnMenuProps {
  columns: ColumnMeta[];
  layout: ColumnLayout;
  onChange: (next: ColumnLayout) => void;
}

/** Popover to show/hide and pin columns, filter them by name and reset the layout. */
export function ColumnMenu({ columns, layout, onChange }: ColumnMenuProps) {
  const { t } = useTranslation('data');
  const [query, setQuery] = useState('');
  const names = columns.map((c) => c.name);
  const l = reconcileLayout(layout, names);
  const hidden = new Set(l.hidden);
  const pinned = new Set(l.pinned);
  const q = query.trim().toLowerCase();
  const shown = l.order.filter((n) => n.toLowerCase().includes(q));

  return (
    <Popover.Root>
      <Popover.Trigger asChild>
        <Button
          size="icon"
          variant="ghost"
          aria-label={t('columns.menu')}
          title={t('columns.menu')}
          className={l.hidden.length + l.pinned.length > 0 ? 'text-accent' : undefined}
        >
          <Columns3 size={13} />
        </Button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content
          align="start"
          sideOffset={4}
          aria-label={t('columns.menu')}
          className={`${menuContentClass} w-64 p-2`}
        >
          <Input
            aria-label={t('columns.filter')}
            placeholder={t('columns.filter')}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            className="mb-2 h-6 text-xs"
          />
          <ul className="max-h-64 overflow-auto" aria-label={t('columns.list')}>
            {shown.map((n) => (
              <li
                key={n}
                className="flex h-6 items-center gap-2 rounded px-1 hover:bg-surface-muted"
              >
                <Checkbox
                  aria-label={n}
                  checked={!hidden.has(n)}
                  onCheckedChange={() => onChange(toggleHidden(l, n, names))}
                />
                <span className="min-w-0 flex-1 truncate text-xs">{n}</span>
                <IconButton
                  label={
                    pinned.has(n) ? `${t('columns.unpin')}: ${n}` : `${t('columns.pin')}: ${n}`
                  }
                  icon={pinned.has(n) ? <PinOff size={12} /> : <Pin size={12} />}
                  onClick={() => onChange(setPinned(l, n, !pinned.has(n), names))}
                />
              </li>
            ))}
          </ul>
          <div className="mt-2 flex justify-between gap-2">
            <Button
              size="sm"
              onClick={() => onChange(showAll(l, names))}
              disabled={l.hidden.length === 0}
            >
              {t('columns.showAll')}
            </Button>
            <Button
              size="sm"
              onClick={() => onChange(resetLayout())}
              disabled={isDefaultLayout(layout, names)}
            >
              {t('columns.reset')}
            </Button>
          </div>
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}

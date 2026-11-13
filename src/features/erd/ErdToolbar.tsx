import {
  Crosshair,
  Download,
  Eye,
  FolderOpen,
  LayoutGrid,
  Maximize,
  Palette,
  RefreshCw,
  Save,
  Search,
  Code2,
  X,
} from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/Button';
import { Checkbox } from '@/components/ui/Checkbox';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from '@/components/ui/DropdownMenu';
import { IconButton } from '@/components/ui/IconButton';
import { Input } from '@/components/ui/Input';
import { Select } from '@/components/ui/Select';
import { Spinner } from '@/components/ui/Spinner';
import type { ExportFormat, ExportTheme } from './exportImage';
import type { Focus } from './useErd';
import type { LayoutOptions } from './model';

const TABLE_COLORS = ['#3b5bdb', '#0b7285', '#2b8a3e', '#e67700', '#c2255c', '#6741d9'];

interface ErdToolbarProps {
  options: LayoutOptions;
  onOptions: (o: LayoutOptions) => void;
  search: string;
  onSearch: (s: string) => void;
  matchCount: number;
  onSubmitSearch: () => void;
  selectedKey: string | null;
  focus: Focus | null;
  onFocus: (f: Focus | null) => void;
  shown: number;
  total: number;
  busy: boolean;
  showDdl: boolean;
  onToggleDdl: () => void;
  canColor: boolean;
  onColor: (color: string | null) => void;
  onRelayout: () => void;
  onFit: () => void;
  onRefresh: () => void;
  onSave: () => void;
  onOpenLayouts: () => void;
  layoutName: string | null;
  onExport: (format: ExportFormat, theme: ExportTheme | 'current') => void;
  exporting: boolean;
}

const FORMATS: ExportFormat[] = ['png', 'svg', 'pdf'];

export function ErdToolbar(p: ErdToolbarProps) {
  const { t } = useTranslation('erd');
  const depths = [1, 2, 3].map((d) => ({
    value: String(d),
    label: t('toolbar.levels', { n: d }),
  }));
  return (
    <div
      role="toolbar"
      aria-label={t('toolbar.label')}
      className="flex min-h-9 shrink-0 flex-wrap items-center gap-1.5 border-b border-border px-2 py-1"
    >
      <div className="relative">
        <Search
          size={12}
          className="pointer-events-none absolute top-1/2 left-2 -translate-y-1/2 text-muted-foreground"
        />
        <Input
          aria-label={t('toolbar.search')}
          placeholder={t('toolbar.searchPlaceholder')}
          value={p.search}
          className="w-52 pl-6"
          onChange={(e) => p.onSearch(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') p.onSubmitSearch();
            if (e.key === 'Escape') p.onSearch('');
          }}
        />
        {p.search && (
          <span className="absolute top-1/2 right-6 -translate-y-1/2 text-[11px] text-muted-foreground">
            {p.matchCount}
          </span>
        )}
        {p.search && (
          <button
            type="button"
            aria-label={t('toolbar.clearSearch')}
            className="absolute top-1/2 right-1.5 -translate-y-1/2 text-muted-foreground hover:text-foreground"
            onClick={() => p.onSearch('')}
          >
            <X size={12} />
          </button>
        )}
      </div>

      {p.focus ? (
        <>
          <Select
            aria-label={t('toolbar.depth')}
            className="w-28"
            value={String(p.focus.depth)}
            options={depths}
            onValueChange={(v) => p.focus && p.onFocus({ ...p.focus, depth: Number(v) })}
          />
          <Button size="sm" variant="primary" onClick={() => p.onFocus(null)}>
            <Eye size={12} /> {t('toolbar.showAll')}
          </Button>
        </>
      ) : (
        <Button
          size="sm"
          disabled={p.selectedKey === null}
          title={t('toolbar.focusHint')}
          onClick={() => p.selectedKey && p.onFocus({ key: p.selectedKey, depth: 1 })}
        >
          <Crosshair size={12} /> {t('toolbar.focus')}
        </Button>
      )}

      <span className="mx-1 h-4 w-px bg-border" aria-hidden />

      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button size="sm">
            <Eye size={12} /> {t('toolbar.view')}
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start" className="min-w-52">
          <DropdownMenuItem
            onSelect={(e) => {
              e.preventDefault();
              p.onOptions({ ...p.options, columns: p.options.columns === 'all' ? 'keys' : 'all' });
            }}
          >
            <Checkbox checked={p.options.columns === 'keys'} tabIndex={-1} aria-hidden />
            {t('toolbar.keysOnly')}
          </DropdownMenuItem>
          <DropdownMenuItem
            onSelect={(e) => {
              e.preventDefault();
              p.onOptions({ ...p.options, showTypes: !p.options.showTypes });
            }}
          >
            <Checkbox checked={p.options.showTypes} tabIndex={-1} aria-hidden />
            {t('toolbar.showTypes')}
          </DropdownMenuItem>
          <DropdownMenuItem
            onSelect={(e) => {
              e.preventDefault();
              p.onOptions({ ...p.options, showViews: !p.options.showViews });
            }}
          >
            <Checkbox checked={p.options.showViews} tabIndex={-1} aria-hidden />
            {t('toolbar.showViews')}
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>

      <Button size="sm" onClick={p.onRelayout} disabled={p.busy}>
        {p.busy ? <Spinner /> : <LayoutGrid size={12} />} {t('toolbar.autoLayout')}
      </Button>
      <IconButton label={t('toolbar.fit')} icon={<Maximize size={13} />} onClick={p.onFit} />
      <IconButton
        label={t('toolbar.refresh')}
        icon={<RefreshCw size={13} />}
        onClick={p.onRefresh}
      />

      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button
            size="icon"
            variant="ghost"
            aria-label={t('toolbar.color')}
            disabled={!p.canColor}
          >
            <Palette size={13} />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start">
          <div className="flex gap-1 p-1.5">
            {TABLE_COLORS.map((c) => (
              <button
                key={c}
                type="button"
                aria-label={c}
                className="size-5 rounded-full border border-border"
                style={{ backgroundColor: c }}
                onClick={() => p.onColor(c)}
              />
            ))}
          </div>
          <DropdownMenuSeparator />
          <DropdownMenuItem onSelect={() => p.onColor(null)}>
            {t('toolbar.colorClear')}
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>

      <span className="mx-1 h-4 w-px bg-border" aria-hidden />

      <Button size="sm" onClick={p.onSave}>
        <Save size={12} /> {t('toolbar.save')}
      </Button>
      <Button size="sm" onClick={p.onOpenLayouts}>
        <FolderOpen size={12} /> {t('toolbar.layouts')}
      </Button>
      {p.layoutName && (
        <span className="max-w-40 truncate text-xs text-muted-foreground" title={p.layoutName}>
          {p.layoutName}
        </span>
      )}

      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button size="sm" disabled={p.exporting || p.total === 0}>
            {p.exporting ? <Spinner /> : <Download size={12} />} {t('toolbar.export')}
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start">
          {FORMATS.map((f) => (
            <DropdownMenuSub key={f}>
              <DropdownMenuSubTrigger>{t(`export.${f}`)}</DropdownMenuSubTrigger>
              <DropdownMenuSubContent>
                <DropdownMenuItem onSelect={() => p.onExport(f, 'current')}>
                  {t('export.currentTheme')}
                </DropdownMenuItem>
                <DropdownMenuItem onSelect={() => p.onExport(f, 'light')}>
                  {t('export.light')}
                </DropdownMenuItem>
                <DropdownMenuItem onSelect={() => p.onExport(f, 'dark')}>
                  {t('export.dark')}
                </DropdownMenuItem>
              </DropdownMenuSubContent>
            </DropdownMenuSub>
          ))}
        </DropdownMenuContent>
      </DropdownMenu>

      <Button
        size="sm"
        variant={p.showDdl ? 'primary' : 'outline'}
        disabled={p.selectedKey === null}
        aria-pressed={p.showDdl}
        onClick={p.onToggleDdl}
      >
        <Code2 size={12} /> {t('toolbar.ddl')}
      </Button>

      <span className="ml-auto text-xs text-muted-foreground" data-testid="erd-count">
        {t('toolbar.count', { shown: p.shown, total: p.total })}
      </span>
    </div>
  );
}

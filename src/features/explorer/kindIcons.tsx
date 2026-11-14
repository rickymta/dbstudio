import {
  Box,
  Columns3,
  Database,
  Eye,
  Folder,
  FolderTree,
  Link2,
  Package,
  Shapes,
  Hash,
  KeyRound,
  Layers,
  ListTree,
  SquareFunction,
  Table2,
  TableProperties,
  Terminal,
  User,
  Zap,
  type LucideIcon,
} from 'lucide-react';
import type { ObjectKind } from '@/lib/types';

const KIND_ICONS: Record<ObjectKind, LucideIcon> = {
  database: Database,
  schema: Layers,
  table: Table2,
  view: Eye,
  materializedView: TableProperties,
  role: User,
  column: Columns3,
  index: ListTree,
  function: SquareFunction,
  procedure: Terminal,
  trigger: Zap,
  sequence: Hash,
  collection: Table2,
  key: KeyRound,
  folder: Folder,
  keyPrefix: FolderTree,
  synonym: Link2,
  package: Package,
  type: Shapes,
  dictionary: Shapes,
};

/** Icon of an object kind; unknown kinds from a newer backend get a generic box. */
export function KindIcon({
  kind,
  size = 14,
  className,
}: {
  kind: ObjectKind;
  size?: number;
  className?: string;
}) {
  const Icon = KIND_ICONS[kind] ?? Box;
  return <Icon size={size} className={className} aria-hidden />;
}

/**
 * The object tree's icons: a Lucide glyph per object kind, tinted with the `--icon-*` tokens
 * (`.tree-icon--<tone>` in workbench.css). Connections show the engine tile instead.
 */
import {
  Boxes,
  CalendarClock,
  Code,
  Columns3,
  Database,
  Eye,
  Folder,
  FolderOpen,
  KeyRound,
  Layers,
  Link2,
  ListChecks,
  ListOrdered,
  type LucideIcon,
  Puzzle,
  ShieldCheck,
  SquareFunction,
  Table2,
  TableProperties,
  UserRound,
  Users,
  Zap,
} from "lucide-react";
import { ICON } from "../../components/icons";
import { EngineBadge } from "../../connections/EngineBadge";
import type { Row } from "./treeModel";

type NodeRow = Extract<Row, { type: "node" }>;

/** Icon and colour tone per `Row.icon` name (object kinds with dashes). */
const KIND_ICONS: Record<string, { Icon: LucideIcon; tone: string }> = {
  database: { Icon: Database, tone: "database" },
  schema: { Icon: Layers, tone: "schema" },
  table: { Icon: Table2, tone: "table" },
  view: { Icon: Eye, tone: "view" },
  "mat-view": { Icon: TableProperties, tone: "view" },
  column: { Icon: Columns3, tone: "column" },
  index: { Icon: KeyRound, tone: "index" },
  "foreign-key": { Icon: Link2, tone: "key" },
  key: { Icon: KeyRound, tone: "key" },
  check: { Icon: ListChecks, tone: "check" },
  function: { Icon: SquareFunction, tone: "routine" },
  procedure: { Icon: Code, tone: "routine" },
  trigger: { Icon: Zap, tone: "trigger" },
  sequence: { Icon: ListOrdered, tone: "sequence" },
  type: { Icon: Boxes, tone: "schema" },
  extension: { Icon: Puzzle, tone: "column" },
  user: { Icon: UserRound, tone: "column" },
  role: { Icon: Users, tone: "column" },
  grant: { Icon: ShieldCheck, tone: "column" },
  event: { Icon: CalendarClock, tone: "trigger" },
};

export function TreeIcon({ row }: { row: NodeRow }) {
  if (row.icon === "connection" && row.engine) {
    return <EngineBadge engine={row.engine} size="sm" />;
  }
  if (row.icon === "folder") {
    const Icon = row.expanded ? FolderOpen : Folder;
    return (
      <span className="tree-icon tree-icon--folder" aria-hidden="true">
        <Icon {...ICON} />
      </span>
    );
  }
  const { Icon, tone } = KIND_ICONS[row.icon] ?? { Icon: Boxes, tone: "column" };
  return (
    <span className={`tree-icon tree-icon--${tone}`} aria-hidden="true">
      <Icon {...ICON} />
    </span>
  );
}

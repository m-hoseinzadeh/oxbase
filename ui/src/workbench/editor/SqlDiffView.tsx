/**
 * A read-only side-by-side SQL diff (Monaco's diff editor, loaded on demand like the SQL editor):
 * the structure sync view shows an object's target DDL on the left and its source DDL on the
 * right.
 */
import { LoaderCircle } from "lucide-react";
import { lazy, Suspense } from "react";
import { ICON_SM } from "../../components/icons";
import type { EditorLanguage } from "./language";

export interface SqlDiffViewProps {
  /** Left side (what is there now: the target). */
  original: string;
  /** Right side (what it becomes: the source). */
  modified: string;
  language: EditorLanguage;
  ariaLabel?: string | undefined;
  testId?: string | undefined;
}

const MonacoDiffView = lazy(() => import("./MonacoDiffView"));

export function SqlDiffView(props: SqlDiffViewProps) {
  return (
    <Suspense
      fallback={
        <div className="sql-editor sql-editor--loading">
          <LoaderCircle {...ICON_SM} className="spin" />
          Loading editor…
        </div>
      }
    >
      <MonacoDiffView {...props} />
    </Suspense>
  );
}

import {
  DockviewReact,
  type DockviewReadyEvent,
  type DockviewTheme,
  type IDockviewPanelProps,
  themeLight,
} from "dockview-react";
import { type FunctionComponent, useEffect, useState } from "react";
import { ConnectionsPanel } from "./panels/ConnectionsPanel";
import { WelcomePanel } from "./panels/WelcomePanel";
import { StatusBar } from "./StatusBar";
import { TableDataPanel } from "./workbench/data/TableDataPanel";
import { TableDesignerPanel } from "./workbench/designer/TableDesignerPanel";
import { QueryEditorPanel } from "./workbench/editor/QueryEditorPanel";
import { HistoryPanel } from "./workbench/history/HistoryPanel";
import {
  type DesignerPanelParams,
  designerTitle,
  newEditorId,
  openQueryEditor,
  type QueryEditorParams,
  setDockviewApi,
  showHistory,
  type TableDataPanelParams,
} from "./workbench/layout";
import { ObjectTreePanel } from "./workbench/tree/ObjectTreePanel";

function QueryPanel({ params }: IDockviewPanelProps<QueryEditorParams>) {
  // A panel restored without params still gets an editor id of its own.
  const [editorId] = useState(() => params.editorId || newEditorId());
  return <QueryEditorPanel {...params} editorId={editorId} />;
}

function TableDataTab({ params }: IDockviewPanelProps<TableDataPanelParams>) {
  const [editorId] = useState(() => params.editorId || newEditorId());
  if (!params.sessionId || !params.table) {
    return <p className="results__empty">This tab lost its table. Open it again from the tree.</p>;
  }
  return <TableDataPanel editorId={editorId} sessionId={params.sessionId} table={params.table} />;
}

function DesignerTab({ params, api }: IDockviewPanelProps<DesignerPanelParams>) {
  if (!params.sessionId || !params.table) {
    return <p className="results__empty">This tab lost its table. Open it again from the tree.</p>;
  }
  return (
    <TableDesignerPanel
      sessionId={params.sessionId}
      table={params.table}
      isNew={params.isNew}
      onTableChanged={(table) => {
        // The tab now designs the created or renamed table: found (and reloaded) by that name.
        // dockview merges params: `isNew` is set explicitly, not left out.
        const next: DesignerPanelParams = { sessionId: params.sessionId, table, isNew: false };
        api.updateParameters(next);
        api.setTitle(designerTitle(next));
      }}
    />
  );
}

const components: Record<string, FunctionComponent<IDockviewPanelProps>> = {
  connections: () => <ConnectionsPanel />,
  objects: () => <ObjectTreePanel />,
  welcome: () => <WelcomePanel />,
  history: () => <HistoryPanel />,
  query: QueryPanel,
  tableData: TableDataTab,
  designer: DesignerTab,
};

// Structural variables come from dockview's light theme; colours are overridden in
// theme.css with our own CSS variables, which follow the OS light/dark preference.
const theme: DockviewTheme = { ...themeLight, name: "oxbase" };

function onReady(event: DockviewReadyEvent) {
  setDockviewApi(event.api);
  const welcome = event.api.addPanel({ id: "welcome", component: "welcome", title: "Welcome" });
  const connections = event.api.addPanel({
    id: "connections",
    component: "connections",
    title: "Connections",
    position: { referencePanel: welcome, direction: "left" },
    initialWidth: 280,
  });
  event.api.addPanel({
    id: "objects",
    component: "objects",
    title: "Objects",
    position: { referencePanel: connections, direction: "below" },
  });
  event.api.addPanel({
    id: "history",
    component: "history",
    title: "History",
    position: { referencePanel: welcome, direction: "below" },
    initialHeight: 200,
    inactive: true,
  });
  welcome.api.setActive();
}

export function App() {
  useEffect(() => () => setDockviewApi(null), []);
  return (
    <div className="app">
      <header className="app__toolbar" role="toolbar" aria-label="Workbench">
        <button type="button" className="button" onClick={() => openQueryEditor()}>
          New query
        </button>
        <button type="button" className="button" onClick={showHistory}>
          History
        </button>
      </header>
      <main className="app__workspace">
        <DockviewReact components={components} onReady={onReady} theme={theme} />
      </main>
      <StatusBar />
    </div>
  );
}

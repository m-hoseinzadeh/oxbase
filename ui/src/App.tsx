import {
  DockviewDefaultTab,
  DockviewReact,
  type DockviewReadyEvent,
  type DockviewTheme,
  type IDockviewPanelHeaderProps,
  type IDockviewPanelProps,
  themeLight,
} from "dockview-react";
import { Database, FileInput, History, Plug, SquarePen } from "lucide-react";
import { type FunctionComponent, useEffect, useState } from "react";
import { ICON } from "./components/icons";
import { requestNewConnection } from "./connections/requests";
import { ConnectionsPanel } from "./panels/ConnectionsPanel";
import { WelcomePanel } from "./panels/WelcomePanel";
import { StatusBar } from "./StatusBar";
import { TableDataPanel } from "./workbench/data/TableDataPanel";
import { TableDesignerPanel } from "./workbench/designer/TableDesignerPanel";
import { QueryEditorPanel } from "./workbench/editor/QueryEditorPanel";
import { HistoryPanel } from "./workbench/history/HistoryPanel";
import {
  activePanelId,
  type CollectionPanelParams,
  type ConsolePanelParams,
  type DesignerPanelParams,
  designerTitle,
  type KeyBrowserParams,
  newEditorId,
  openQueryEditor,
  type PubSubPanelParams,
  type QueryEditorParams,
  type SyncPanelParams,
  setDockviewApi,
  showHistory,
  type TableDataPanelParams,
} from "./workbench/layout";
import { CollectionPanel } from "./workbench/nosql/CollectionPanel";
import { ConsolePanel } from "./workbench/nosql/console/ConsolePanel";
import { KeyBrowserPanel } from "./workbench/nosql/keys/KeyBrowserPanel";
import { PubSubPanel } from "./workbench/nosql/pubsub/PubSubPanel";
import { installShortcuts } from "./workbench/shortcuts";
import { DataSyncPanel } from "./workbench/sync/DataSyncPanel";
import { StructureSyncPanel } from "./workbench/sync/StructureSyncPanel";
import { SyncHost } from "./workbench/sync/SyncHost";
import { SyncMenu } from "./workbench/sync/SyncMenu";
import { openImport } from "./workbench/transfer/store";
import { TransferHost } from "./workbench/transfer/TransferHost";
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

function CollectionTab({ params }: IDockviewPanelProps<CollectionPanelParams>) {
  const [editorId] = useState(() => params.editorId || newEditorId());
  if (!params.sessionId || !params.collection) {
    return (
      <p className="results__empty">This tab lost its collection. Open it again from the tree.</p>
    );
  }
  return (
    <CollectionPanel
      editorId={editorId}
      sessionId={params.sessionId}
      collection={params.collection}
    />
  );
}

function KeysTab({ params }: IDockviewPanelProps<KeyBrowserParams>) {
  const [editorId] = useState(() => params.editorId || newEditorId());
  if (!params.sessionId) {
    return (
      <p className="results__empty">This tab lost its connection. Open it again from the tree.</p>
    );
  }
  return <KeyBrowserPanel {...params} editorId={editorId} />;
}

function ConsoleTab({ params }: IDockviewPanelProps<ConsolePanelParams>) {
  const [editorId] = useState(() => params.editorId || newEditorId());
  if (!params.sessionId) {
    return (
      <p className="results__empty">This tab lost its connection. Open it again from the tree.</p>
    );
  }
  return <ConsolePanel {...params} editorId={editorId} />;
}

function PubSubTab({ params }: IDockviewPanelProps<PubSubPanelParams>) {
  if (!params.sessionId) {
    return (
      <p className="results__empty">This tab lost its connection. Open it again from the tree.</p>
    );
  }
  return <PubSubPanel sessionId={params.sessionId} />;
}

function StructureSyncTab({ params }: IDockviewPanelProps<SyncPanelParams>) {
  return <StructureSyncPanel params={params ?? {}} />;
}

function DataSyncTab({ params }: IDockviewPanelProps<SyncPanelParams>) {
  return <DataSyncPanel params={params ?? {}} />;
}

const components: Record<string, FunctionComponent<IDockviewPanelProps>> = {
  connections: () => <ConnectionsPanel />,
  objects: () => <ObjectTreePanel />,
  welcome: () => <WelcomePanel />,
  history: () => <HistoryPanel />,
  query: QueryPanel,
  tableData: TableDataTab,
  designer: DesignerTab,
  collection: CollectionTab,
  keys: KeysTab,
  console: ConsoleTab,
  pubsub: PubSubTab,
  structureSync: StructureSyncTab,
  dataSync: DataSyncTab,
};

/**
 * Every tab: dockview's React default tab, which closes on a middle click (press and release
 * of the middle button) like its close button does. Without `defaultTabComponent` dockview
 * draws its plain DOM tab, which has no middle-click close. The middle button's press is
 * cancelled so the webview does not start auto-scrolling.
 */
function Tab(props: IDockviewPanelHeaderProps) {
  return (
    <DockviewDefaultTab
      {...props}
      onMouseDown={(event) => {
        if (event.button === 1) event.preventDefault();
      }}
    />
  );
}

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
  // F5, Ctrl+Enter, ... act on the active tab (./workbench/shortcuts).
  useEffect(() => installShortcuts(activePanelId), []);
  return (
    <div className="app">
      <header className="app__bar">
        <div className="app__brand">
          <span className="app__mark" aria-hidden="true">
            <Database size={14} strokeWidth={2.25} />
          </span>
          <span className="app__name">Oxbase</span>
        </div>
        <div className="app__toolbar" role="toolbar" aria-label="Workbench">
          <button
            type="button"
            className="button button--ghost"
            title="New connection"
            onClick={() => requestNewConnection()}
          >
            <Plug {...ICON} />
            New connection
          </button>
          <button
            type="button"
            className="button button--ghost"
            title="New query"
            onClick={() => openQueryEditor()}
          >
            <SquarePen {...ICON} />
            New query
          </button>
          <span className="toolbar__separator" aria-hidden="true" />
          <button type="button" className="button button--ghost" onClick={() => openImport()}>
            <FileInput {...ICON} />
            Import…
          </button>
          <SyncMenu />
          <button type="button" className="button button--ghost" onClick={showHistory}>
            <History {...ICON} />
            History
          </button>
        </div>
      </header>
      <main className="app__workspace">
        <DockviewReact
          components={components}
          defaultTabComponent={Tab}
          onReady={onReady}
          theme={theme}
        />
      </main>
      <StatusBar />
      <TransferHost />
      <SyncHost />
    </div>
  );
}

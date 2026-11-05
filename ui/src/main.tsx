import "@fontsource-variable/inter";
import "@fontsource-variable/jetbrains-mono";
import "dockview-react/dist/styles/dockview.css";
import "@glideapps/glide-data-grid/dist/index.css";
import "./theme.css";
import "./workbench/workbench.css";
import "./workbench/m3.css";
import "./workbench/transfer/transfer.css";
import "./workbench/sync/sync.css";
import "./workbench/editor/insight/insight.css";
import "./workbench/nosql/nosql.css";

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { installNativeMenuGuard } from "./workbench/native-menu";
import { initTheme } from "./workbench/theme-bridge";
import { initZoom } from "./workbench/zoom";

initTheme();
initZoom();
installNativeMenuGuard();

const root = document.getElementById("root");
if (!root) {
  throw new Error("#root element missing from index.html");
}

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);

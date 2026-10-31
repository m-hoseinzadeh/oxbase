import "dockview-react/dist/styles/dockview.css";
import "@glideapps/glide-data-grid/dist/index.css";
import "./theme.css";
import "./workbench/workbench.css";

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";

const root = document.getElementById("root");
if (!root) {
  throw new Error("#root element missing from index.html");
}

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);

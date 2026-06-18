import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import Settings from "./Settings";
import Console from "./Console";
import { validate } from "./lib/data";
import "./styles.css";

// One bundle serves every webview window; the tray opens each via a `?view=` query param.
const view = new URLSearchParams(window.location.search).get("view");
const isSettings = view === "settings";
const isConsole = view === "console";

// Fail loudly in dev if a hand-edit to the Ethics Guide JSON datasets broke their shape/counts.
if (!isSettings && !isConsole && import.meta.env.DEV) {
  try {
    validate();
  } catch (e) {
    console.error(e);
  }
}

const root = document.getElementById("root");
if (!root) throw new Error("missing #root element");

ReactDOM.createRoot(root).render(
  <React.StrictMode>
    {isSettings ? <Settings /> : isConsole ? <Console /> : <App />}
  </React.StrictMode>,
);

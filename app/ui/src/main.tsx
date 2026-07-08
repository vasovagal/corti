import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import Queue from "./Queue";
import Settings from "./Settings";
import Console from "./Console";
import { validate } from "./lib/data";
import "./styles.css";

// One bundle serves every webview window; the tray picks a view via the URL query
// (`index.html?view=settings`, `?view=console`, `?view=queue`; no query = the Ethics Guide).
const view = new URLSearchParams(window.location.search).get("view");

// Fail loudly in dev if a hand-edit to the Ethics Guide JSON datasets broke their shape/counts.
if (view === null && import.meta.env.DEV) {
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
    {view === "settings" ? (
      <Settings />
    ) : view === "console" ? (
      <Console />
    ) : view === "queue" ? (
      <Queue />
    ) : (
      <App />
    )}
  </React.StrictMode>,
);

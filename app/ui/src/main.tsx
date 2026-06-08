import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import Settings from "./Settings";
import { validate } from "./lib/data";
import "./styles.css";

// One bundle serves both webview windows; the tray opens Settings with `index.html?view=settings`.
const isSettings = new URLSearchParams(window.location.search).get("view") === "settings";

// Fail loudly in dev if a hand-edit to the Ethics Guide JSON datasets broke their shape/counts.
if (!isSettings && import.meta.env.DEV) {
  try {
    validate();
  } catch (e) {
    console.error(e);
  }
}

const root = document.getElementById("root");
if (!root) throw new Error("missing #root element");

ReactDOM.createRoot(root).render(
  <React.StrictMode>{isSettings ? <Settings /> : <App />}</React.StrictMode>,
);

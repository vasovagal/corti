import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { validate } from "./lib/data";
import "./styles.css";

// Fail loudly in dev if a hand-edit to the JSON datasets broke their shape/counts.
if (import.meta.env.DEV) {
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
    <App />
  </React.StrictMode>,
);

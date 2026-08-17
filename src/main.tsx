import React from "react";
import ReactDOM from "react-dom/client";

import App from "./App";
// Tailwind v4 entry point and the whole design-token layer. Must be imported
// before any component stylesheet so component rules win the cascade.
import "./index.css";

const container = document.getElementById("root");
if (!container) {
  throw new Error("index.html is missing #root; the WebView has nothing to mount into.");
}

ReactDOM.createRoot(container).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);

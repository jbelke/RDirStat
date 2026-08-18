import React from "react";
import ReactDOM from "react-dom/client";

import App from "./App";
import { ErrorBoundary } from "./components/ErrorBoundary";
// Tailwind v4 entry point and the whole design-token layer. Must be imported
// before any component stylesheet so component rules win the cascade.
import "./index.css";

const container = document.getElementById("root");
if (!container) {
  throw new Error("index.html is missing #root; the WebView has nothing to mount into.");
}

// Outside React's render phase entirely, so the boundary below cannot see it.
// A rejected promise with no handler is otherwise completely silent in a
// WKWebView — no banner, no console entry the user can be asked for, nothing.
window.addEventListener("unhandledrejection", (event) => {
  console.error("unhandled rejection", event.reason);
});

// The boundary sits *inside* StrictMode and *outside* App, so it still catches
// a throw from the providers themselves — a QueryClient that fails to build
// would otherwise take the window down before any in-App boundary mounted.
ReactDOM.createRoot(container).render(
  <React.StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </React.StrictMode>,
);

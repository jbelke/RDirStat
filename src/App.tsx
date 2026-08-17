/**
 * Providers, then whichever surface this window is.
 *
 * The `QueryClient` is created once with `useState`'s lazy initialiser rather
 * than at module scope. Module scope looks simpler and is wrong under Vite HMR:
 * the module re-evaluates on edit, the client is replaced, and every in-flight
 * generation-keyed query is orphaned mid-scan.
 *
 * `TooltipProvider` wraps everything so Radix can share its hover-delay state —
 * moving between adjacent tree rows then shows the second tooltip immediately
 * instead of restarting the delay on each row.
 *
 * **Two windows, one bundle.** The menu-bar panel (`src-tauri/src/tray.rs`) is a
 * second webview onto the same frontend, told apart by `?window=tray`. Sharing
 * the bundle is what keeps the panel honest: it reads the same commands through
 * the same adapter, so it cannot drift into showing a number the main window
 * disagrees with. Each window gets its **own** `QueryClient`, because they are
 * separate JavaScript realms — there is no shared cache to be had, and
 * pretending otherwise would just be a confusing comment.
 */

import { QueryClientProvider } from "@tanstack/react-query";
import { useState } from "react";

import { AppShell } from "@/components/AppShell";
import { TrayPanel } from "@/components/TrayPanel";
import { TooltipProvider } from "@/components/ui/tooltip";
import { createQueryClient } from "@/lib/queries";

/** Whether this webview is the menu-bar panel rather than the main window. */
function isTrayPanel(): boolean {
  if (typeof window === "undefined") return false;
  return new URLSearchParams(window.location.search).get("window") === "tray";
}

export default function App() {
  const [client] = useState(createQueryClient);
  const [tray] = useState(isTrayPanel);

  return (
    <QueryClientProvider client={client}>
      <TooltipProvider>{tray ? <TrayPanel /> : <AppShell />}</TooltipProvider>
    </QueryClientProvider>
  );
}

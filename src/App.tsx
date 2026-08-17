/**
 * Providers, then the shell.
 *
 * The `QueryClient` is created once with `useState`'s lazy initialiser rather
 * than at module scope. Module scope looks simpler and is wrong under Vite HMR:
 * the module re-evaluates on edit, the client is replaced, and every in-flight
 * generation-keyed query is orphaned mid-scan.
 *
 * `TooltipProvider` wraps everything so Radix can share its hover-delay state —
 * moving between adjacent tree rows then shows the second tooltip immediately
 * instead of restarting the delay on each row.
 */

import { QueryClientProvider } from "@tanstack/react-query";
import { useState } from "react";

import { AppShell } from "@/components/AppShell";
import { TooltipProvider } from "@/components/ui/tooltip";
import { createQueryClient } from "@/lib/queries";

export default function App() {
  const [client] = useState(createQueryClient);

  return (
    <QueryClientProvider client={client}>
      <TooltipProvider>
        <AppShell />
      </TooltipProvider>
    </QueryClientProvider>
  );
}

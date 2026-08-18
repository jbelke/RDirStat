/**
 * The last thing between a render-phase throw and a blank window.
 *
 * React unmounts the whole tree when a render throws and nothing catches it.
 * In a browser that leaves a white page the user can reload; in a WKWebView
 * with no chrome it leaves a **grey rectangle with no reload affordance**, and
 * the only way out is Force Quit from the Dock. The app is otherwise still
 * running — the Rust side keeps scanning, the tray keeps counting — so the
 * user is told nothing while work continues invisibly.
 *
 * That asymmetry is the whole argument for this file. A boundary cannot make
 * the bug not happen, but it turns "the app died" into "this view died, here
 * is what it said, and here is a button", which is the difference between a
 * bug report we can act on and one that reads "it just disappeared".
 *
 * ## Why a class component
 *
 * `getDerivedStateFromError` and `componentDidCatch` have no hook equivalent.
 * React has never shipped one, and every library that offers a hook-shaped API
 * is a class underneath. This is the one place in the codebase where a class
 * is not a style choice.
 *
 * ## What it deliberately does not do
 *
 * It does not retry automatically, and it does not swallow. A render that
 * throws once will usually throw again on the same state, so a silent retry
 * loop would spin the CPU and hide the fault. `console.error` is kept so the
 * stack is in the Web Inspector, which is where anyone debugging this will
 * already be looking.
 *
 * It also does not catch async failures — event handlers, `invoke()`
 * rejections, and effects that reject are outside React's render phase by
 * design. Those already surface as typed errors in the scan banner
 * (`AppShell`) and the alert chips (`ScanAlerts`); the global
 * `unhandledrejection` listener below exists only so that one which escapes
 * both is visible in the log rather than silent.
 */

import { TriangleAlert } from "lucide-react";
import { Component, type ErrorInfo, type ReactNode } from "react";

import { Button } from "@/components/ui/button";

interface Props {
  children: ReactNode;
}

interface State {
  error: Error | null;
}

export class ErrorBoundary extends Component<Props, State> {
  override state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  override componentDidCatch(error: Error, info: ErrorInfo): void {
    // Kept on purpose: this is the only record of the component stack, and the
    // Web Inspector is where the next person to see this screen will be.
    console.error("render failed", error, info.componentStack);
  }

  override render(): ReactNode {
    const { error } = this.state;
    if (!error) return this.props.children;

    return (
      <div
        role="alert"
        className="flex h-screen w-screen flex-col items-center justify-center gap-4 bg-background p-8 text-center"
      >
        <TriangleAlert className="size-8 text-destructive" aria-hidden="true" />
        <div className="space-y-2">
          <h1 className="text-lg font-semibold">This view stopped rendering.</h1>
          <p className="max-w-prose text-sm text-muted-foreground">
            The scan itself is unaffected — nothing was written and nothing was deleted. Reloading
            rebuilds the window from the data already on disk.
          </p>
        </div>
        {/* The message, not the stack: the stack is in the Web Inspector, and a
            wall of minified frames on screen helps nobody read the one line
            that names the failure. */}
        <pre className="max-w-prose overflow-x-auto rounded-md bg-muted p-3 text-left text-xs text-muted-foreground">
          {error.message}
        </pre>
        <Button onClick={() => window.location.reload()}>Reload the window</Button>
      </div>
    );
  }
}

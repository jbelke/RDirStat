/**
 * Turning a stored theme choice into a class on `<html>`.
 *
 * The stylesheets already had all three states before this existed: `.light`
 * and `.dark` force, and every `prefers-color-scheme: dark` block is written
 * `:root:not(.light)` so the OS only wins when nothing is forcing. So the
 * preference needed no new CSS — only somebody to set the class.
 *
 * **"System" is the absence of a class, not a third class.** Writing `.light`
 * for someone who chose System would pin them to whatever macOS happened to
 * report at that moment, and they would stop following it at sunset. The whole
 * value of System is that it keeps tracking, which it can only do if we say
 * nothing.
 */

import type { ThemeChoice } from "@/lib/ipc";

/** The classes this owns. Removed together so a change never leaves both on. */
const FORCED = ["light", "dark"] as const;

/**
 * Applies `choice` to the document element.
 *
 * Idempotent, and safe to call before the stored preference has loaded — the
 * default is System, which is the no-class state the document already starts
 * in, so there is no flash of the wrong scheme while the setting is read.
 */
export function applyTheme(choice: ThemeChoice, element: HTMLElement = document.documentElement): void {
  element.classList.remove(...FORCED);
  if (choice === "system") return;
  element.classList.add(choice);
}

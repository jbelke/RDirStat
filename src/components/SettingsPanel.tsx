/**
 * In-app preferences.
 *
 * ## Only what exists
 *
 * The panel says nothing about preferences that do not exist yet. That is the same rule the tray panel and the
 * storage panel already follow: a settings page that lists unbuilt options
 * makes the built ones look provisional, and roadmap belongs in the repository
 * rather than in a surface someone opened to change something now.
 *
 * ## Why the theme is stored in Rust and not in the browser
 *
 * It would be one line to keep this in `localStorage`. It is stored in
 * `settings.json` beside the snapshot directory instead, because the webview's
 * storage is a cache the OS may clear and the app already owns a preferences
 * file with atomic writes. A preference that silently forgets itself is worse
 * than one that takes an IPC round trip to read.
 */

import { Loader2, MonitorSmartphone, Moon, Sun } from "lucide-react";

import { SegmentedControl } from "@/components/SegmentedControl";
import { SyncSchedules } from "@/components/SyncSchedules";
import type { ThemeChoice } from "@/lib/ipc";
import { cn } from "@/lib/utils";

const THEME_OPTIONS = [
  {
    value: "system" as ThemeChoice,
    label: <MonitorSmartphone aria-hidden />,
    srLabel: "Follow the system",
    title: "Follow macOS, including when it changes at sunset",
  },
  {
    value: "light" as ThemeChoice,
    label: <Sun aria-hidden />,
    srLabel: "Light",
    title: "Always light, whatever macOS is doing",
  },
  {
    value: "dark" as ThemeChoice,
    label: <Moon aria-hidden />,
    srLabel: "Dark",
    title: "Always dark, whatever macOS is doing",
  },
];

export interface SettingsPanelProps {
  theme: ThemeChoice;
  onThemeChange: (theme: ThemeChoice) => void;
  /** True while a change is being written. */
  saving?: boolean;
  error?: string | null;
  className?: string;
}

export function SettingsPanel({
  theme,
  onThemeChange,
  saving = false,
  error = null,
  className,
}: SettingsPanelProps) {
  return (
    <div className={cn("flex min-h-0 flex-1 flex-col overflow-auto p-4", className)}>
      <Row
        label="Appearance"
        hint={
          theme === "system"
            ? "Following macOS. Changes with the system, including on a schedule."
            : `Always ${theme}, whatever macOS is set to.`
        }
      >
        <div className="flex items-center gap-2">
          <SegmentedControl
            label="Colour scheme"
            options={THEME_OPTIONS}
            value={theme}
            onChange={onThemeChange}
          />
          {saving && <Loader2 aria-hidden className="size-3.5 animate-spin text-muted-foreground" />}
        </div>
      </Row>

      {error !== null && <p className="mt-3 text-xs text-destructive">{error}</p>}

      <SyncSchedules className="mt-3" />
    </div>
  );
}

function Row({
  label,
  hint,
  children,
}: {
  label: string;
  hint: string;
  children: React.ReactNode;
}) {
  return (
    <div className="flex flex-col gap-2 rounded border border-border/60 p-3">
      <div className="flex items-baseline justify-between gap-4">
        <span className="text-sm font-medium">{label}</span>
        {children}
      </div>
      <p className="text-xs text-muted-foreground">{hint}</p>
    </div>
  );
}

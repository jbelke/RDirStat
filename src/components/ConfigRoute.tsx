/**
 * One page for everything you configure, in four tabs.
 *
 * Before this, "settings" was three unrelated places: the storage report (which
 * the titlebar gear opened, and which is a report rather than a setting), the
 * remote destination editor (buried above a live transfer queue), and nowhere
 * at all for preferences, which did not exist. The gear pointed at the closest
 * thing available rather than at a settings page.
 *
 * ## The tab is controlled from outside
 *
 * `tab` and `onTabChange` are props rather than internal state, and that is the
 * whole reason this component takes any. Two entry points open this page — the
 * titlebar gear and the menu-bar gear — and both are labelled *Settings*. If
 * the page owned its own tab it would open on whatever it defaulted to, so a
 * control labelled Settings would land the user on a storage report. The rail
 * opens Stored data; the gears open Settings; each caller says which, because
 * only the caller knows what it promised.
 *
 * ## Why configuration and operation were separated
 *
 * The destination editor moved here out of the transfers page. Destinations are
 * something you set up once and forget; the queue is something you watch. The
 * editor sat above the queue, which put the twice-a-year thing permanently
 * above the thing you actually came to look at.
 *
 * Its former neighbour still needs to reach it, though — the transfers page has
 * a destination picker that is empty until one exists, and its copy used to say
 * "add a destination above". Nothing is above it now, so that page is given a
 * way to send the user here instead of a dead end.
 */

import { Database, Info, Server, SlidersHorizontal } from "lucide-react";

import { AboutPanel } from "@/components/AboutPanel";
import { RemoteTargets } from "@/components/RemoteTargets";
import { SettingsPanel, type SettingsPanelProps } from "@/components/SettingsPanel";
import { StoragePanel, type StoragePanelProps } from "@/components/StoragePanel";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";

/** Which tab is showing. Also the value the two gears ask for by name. */
export type ConfigTab = "storage" | "remote" | "settings" | "about";

export interface ConfigRouteProps {
  tab: ConfigTab;
  onTabChange: (tab: ConfigTab) => void;
  storage: StoragePanelProps;
  settings: Omit<SettingsPanelProps, "className">;
  version: string;
  onOpenUrl: (url: string) => void;
  className?: string;
}

export function ConfigRoute({
  tab,
  onTabChange,
  storage,
  settings,
  version,
  onOpenUrl,
  className,
}: ConfigRouteProps) {
  return (
    <Tabs
      value={tab}
      onValueChange={(next) => onTabChange(next as ConfigTab)}
      className={cn("flex min-h-0 flex-1 flex-col", className)}
    >
      <TabsList className="shrink-0 px-4 pt-3">
        <TabsTrigger value="storage">
          <Database aria-hidden />
          Stored data
        </TabsTrigger>
        <TabsTrigger value="remote">
          <Server aria-hidden />
          Remote data
        </TabsTrigger>
        <TabsTrigger value="settings">
          <SlidersHorizontal aria-hidden />
          Settings
        </TabsTrigger>
        <TabsTrigger value="about">
          <Info aria-hidden />
          About
        </TabsTrigger>
      </TabsList>

      {/* Each panel keeps its own heading and scrolling. The tab strip is
        * navigation, not a frame, and wrapping every panel in a second titled
        * container would stack two headings saying the same thing. */}
      <TabsContent value="storage" className="flex min-h-0 flex-col">
        <StoragePanel {...storage} />
      </TabsContent>
      <TabsContent value="remote" className="flex min-h-0 flex-col overflow-auto p-4">
        <RemoteTargets />
      </TabsContent>
      <TabsContent value="settings" className="flex min-h-0 flex-col">
        <SettingsPanel {...settings} />
      </TabsContent>
      <TabsContent value="about" className="flex min-h-0 flex-col">
        <AboutPanel version={version} onOpenUrl={onOpenUrl} />
      </TabsContent>
    </Tabs>
  );
}

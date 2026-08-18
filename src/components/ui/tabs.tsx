/**
 * Tabs, over Radix's primitive.
 *
 * The sibling of `dropdown-menu.tsx` and written for the same reason: what is
 * easy to get wrong here is not the styling. It is the roving tabindex, the
 * arrow-key and Home/End movement, the `aria-controls`/`aria-labelledby` pair
 * between each tab and its panel, and the fact that a tab list is one stop in
 * the tab order rather than four. A row of `<button aria-selected>` looks
 * identical and behaves worse, which is exactly the trade this codebase
 * already refused for the segmented control.
 *
 * Deliberately *not* the segmented control, despite the family resemblance.
 * That one is a radio group — it chooses a value. This one chooses which panel
 * is on screen, and the semantics a screen reader announces should say so.
 */

import { Tabs as TabsPrimitive } from "radix-ui";
import type { ComponentProps } from "react";

import { cn } from "@/lib/utils";

export const Tabs = TabsPrimitive.Root;

export function TabsList({ className, ...props }: ComponentProps<typeof TabsPrimitive.List>) {
  return (
    <TabsPrimitive.List
      className={cn("inline-flex items-center gap-1 border-b border-border/60", className)}
      {...props}
    />
  );
}

export function TabsTrigger({ className, ...props }: ComponentProps<typeof TabsPrimitive.Trigger>) {
  return (
    <TabsPrimitive.Trigger
      className={cn(
        // The selected tab is joined to its panel by a border that overlaps the
        // list's own, which is what makes it read as a tab rather than as a
        // pressed button sitting above unrelated content.
        "-mb-px cursor-default rounded-t-md border-b-2 border-transparent px-3 py-1.5 text-xs",
        "text-muted-foreground transition-colors hover:text-foreground",
        "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
        "data-[state=active]:border-brand data-[state=active]:text-foreground",
        "[&_svg]:size-3.5 [&_svg]:shrink-0",
        className,
      )}
      {...props}
    />
  );
}

export function TabsContent({ className, ...props }: ComponentProps<typeof TabsPrimitive.Content>) {
  return (
    <TabsPrimitive.Content
      className={cn("min-h-0 flex-1 focus-visible:outline-none", className)}
      {...props}
    />
  );
}

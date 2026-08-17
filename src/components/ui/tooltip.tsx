/**
 * Tooltip, over Radix's primitive.
 *
 * Radix rather than a `title` attribute because the hover tooltip in the
 * hierarchy views has to carry a path plus two byte counts plus a category —
 * a native `title` cannot be styled, cannot be read reliably by VoiceOver on
 * macOS, and appears after a fixed OS delay this app does not control.
 *
 * `Provider` is mounted once at the app root. Radix shares its hover-delay
 * state through it, which is what makes moving between adjacent tree rows show
 * the second tooltip instantly instead of restarting the 700 ms timer on every
 * row.
 */

import { Tooltip as TooltipPrimitive } from "radix-ui";
import type { ComponentProps } from "react";

import { cn } from "@/lib/utils";

export function TooltipProvider({
  delayDuration = 400,
  skipDelayDuration = 300,
  ...props
}: ComponentProps<typeof TooltipPrimitive.Provider>) {
  return (
    <TooltipPrimitive.Provider
      delayDuration={delayDuration}
      skipDelayDuration={skipDelayDuration}
      {...props}
    />
  );
}

export const Tooltip = TooltipPrimitive.Root;
export const TooltipTrigger = TooltipPrimitive.Trigger;

export function TooltipContent({
  className,
  sideOffset = 6,
  children,
  ...props
}: ComponentProps<typeof TooltipPrimitive.Content>) {
  return (
    <TooltipPrimitive.Portal>
      <TooltipPrimitive.Content
        sideOffset={sideOffset}
        className={cn(
          "z-50 max-w-sm overflow-hidden rounded-md border border-border",
          "bg-popover px-2.5 py-1.5 text-xs text-popover-foreground shadow-lg",
          "animate-in fade-in-0 zoom-in-95",
          "data-[state=closed]:animate-out data-[state=closed]:fade-out-0 data-[state=closed]:zoom-out-95",
          className,
        )}
        {...props}
      >
        {children}
      </TooltipPrimitive.Content>
    </TooltipPrimitive.Portal>
  );
}

/**
 * Dropdown menu, over Radix's primitive.
 *
 * The sibling of `context-menu.tsx` and deliberately styled from the same two
 * class strings: a menu opened from a button and a menu opened by right-click
 * should be the same object as far as the user is concerned, and duplicating
 * the surface styling is how they drift into looking like two different
 * widgets.
 *
 * Radix rather than a hand-rolled popover because the parts that are easy to
 * get wrong are the parts it already does: focus moves into the menu on open
 * and back to the trigger on close, Escape and outside-click dismiss, typeahead
 * and arrow keys work, and the content is portalled so it is not clipped by the
 * titlebar's `overflow-hidden`. A hand-rolled version of this is not 30 lines,
 * it is 30 lines plus every bug it does not know about yet.
 */

import { DropdownMenu as DropdownMenuPrimitive } from "radix-ui";
import { Check } from "lucide-react";
import type { ComponentProps } from "react";

import { cn } from "@/lib/utils";

export const DropdownMenu = DropdownMenuPrimitive.Root;
export const DropdownMenuTrigger = DropdownMenuPrimitive.Trigger;
export const DropdownMenuGroup = DropdownMenuPrimitive.Group;
export const DropdownMenuRadioGroup = DropdownMenuPrimitive.RadioGroup;

const menuSurface = [
  "z-50 min-w-[12rem] overflow-hidden rounded-md border border-border",
  "bg-popover p-1 text-popover-foreground shadow-xl",
  "animate-in fade-in-0 zoom-in-95",
].join(" ");

const menuItem = [
  "relative flex cursor-default select-none items-center gap-2 rounded-sm",
  "px-2 py-1.5 text-sm outline-none",
  "focus:bg-accent focus:text-accent-foreground",
  "data-[disabled]:pointer-events-none data-[disabled]:opacity-50",
  "[&_svg]:size-4 [&_svg]:shrink-0",
].join(" ");

export function DropdownMenuContent({
  className,
  sideOffset = 6,
  ...props
}: ComponentProps<typeof DropdownMenuPrimitive.Content>) {
  return (
    <DropdownMenuPrimitive.Portal>
      <DropdownMenuPrimitive.Content
        sideOffset={sideOffset}
        className={cn(menuSurface, className)}
        {...props}
      />
    </DropdownMenuPrimitive.Portal>
  );
}

export function DropdownMenuItem({
  className,
  ...props
}: ComponentProps<typeof DropdownMenuPrimitive.Item>) {
  return <DropdownMenuPrimitive.Item className={cn(menuItem, className)} {...props} />;
}

/**
 * A radio item, for a menu that reports *which one is current* rather than
 * offering a list of commands. The indicator column is reserved on every row,
 * checked or not, so the labels do not shift when the selection moves.
 */
export function DropdownMenuRadioItem({
  className,
  children,
  ...props
}: ComponentProps<typeof DropdownMenuPrimitive.RadioItem>) {
  return (
    <DropdownMenuPrimitive.RadioItem className={cn(menuItem, "pl-7", className)} {...props}>
      <span className="absolute left-2 flex size-3.5 items-center justify-center">
        <DropdownMenuPrimitive.ItemIndicator>
          <Check className="size-3.5" />
        </DropdownMenuPrimitive.ItemIndicator>
      </span>
      {children}
    </DropdownMenuPrimitive.RadioItem>
  );
}

export function DropdownMenuLabel({
  className,
  ...props
}: ComponentProps<typeof DropdownMenuPrimitive.Label>) {
  return (
    <DropdownMenuPrimitive.Label
      className={cn("px-2 py-1.5 text-xs font-medium text-muted-foreground", className)}
      {...props}
    />
  );
}

export function DropdownMenuSeparator({
  className,
  ...props
}: ComponentProps<typeof DropdownMenuPrimitive.Separator>) {
  return (
    <DropdownMenuPrimitive.Separator
      className={cn("-mx-1 my-1 h-px bg-border", className)}
      {...props}
    />
  );
}

/**
 * Context menu, over Radix's primitive.
 *
 * This is where the two destructive-capable actions live. docs/05-UI.md:
 * "Destructive actions live in the details panel and the context menu only, so
 * Trash is never one stray click away from a multi-selection", and there is no
 * Delete-key binding anywhere in the app.
 *
 * `ContextMenuItem` therefore takes a `variant` rather than leaving styling to
 * the caller: a Trash item that is not visibly destructive is a defect, and
 * making it the component's job means a new call site cannot forget.
 */

import { ContextMenu as ContextMenuPrimitive } from "radix-ui";
import { Check, ChevronRight } from "lucide-react";
import type { ComponentProps } from "react";

import { cn } from "@/lib/utils";

export const ContextMenu = ContextMenuPrimitive.Root;
export const ContextMenuTrigger = ContextMenuPrimitive.Trigger;
export const ContextMenuGroup = ContextMenuPrimitive.Group;
export const ContextMenuSub = ContextMenuPrimitive.Sub;
export const ContextMenuRadioGroup = ContextMenuPrimitive.RadioGroup;

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

export function ContextMenuContent({
  className,
  ...props
}: ComponentProps<typeof ContextMenuPrimitive.Content>) {
  return (
    <ContextMenuPrimitive.Portal>
      <ContextMenuPrimitive.Content className={cn(menuSurface, className)} {...props} />
    </ContextMenuPrimitive.Portal>
  );
}

export type ContextMenuItemProps = ComponentProps<typeof ContextMenuPrimitive.Item> & {
  /** `destructive` is not decoration: it is the only visual warning before Trash. */
  variant?: "default" | "destructive";
  inset?: boolean;
};

export function ContextMenuItem({
  className,
  variant = "default",
  inset,
  ...props
}: ContextMenuItemProps) {
  return (
    <ContextMenuPrimitive.Item
      data-variant={variant}
      className={cn(
        menuItem,
        inset === true && "pl-8",
        variant === "destructive" &&
          "text-destructive focus:bg-destructive/15 focus:text-destructive [&_svg]:text-destructive",
        className,
      )}
      {...props}
    />
  );
}

export function ContextMenuCheckboxItem({
  className,
  children,
  checked,
  ...props
}: ComponentProps<typeof ContextMenuPrimitive.CheckboxItem>) {
  return (
    <ContextMenuPrimitive.CheckboxItem
      checked={checked}
      className={cn(menuItem, "pl-8", className)}
      {...props}
    >
      <span className="absolute left-2 flex size-4 items-center justify-center">
        <ContextMenuPrimitive.ItemIndicator>
          <Check className="size-3.5" />
        </ContextMenuPrimitive.ItemIndicator>
      </span>
      {children}
    </ContextMenuPrimitive.CheckboxItem>
  );
}

export function ContextMenuSubTrigger({
  className,
  children,
  ...props
}: ComponentProps<typeof ContextMenuPrimitive.SubTrigger>) {
  return (
    <ContextMenuPrimitive.SubTrigger
      className={cn(menuItem, "data-[state=open]:bg-accent", className)}
      {...props}
    >
      {children}
      <ChevronRight className="ml-auto size-4" />
    </ContextMenuPrimitive.SubTrigger>
  );
}

export function ContextMenuSubContent({
  className,
  ...props
}: ComponentProps<typeof ContextMenuPrimitive.SubContent>) {
  return <ContextMenuPrimitive.SubContent className={cn(menuSurface, className)} {...props} />;
}

export function ContextMenuLabel({
  className,
  ...props
}: ComponentProps<typeof ContextMenuPrimitive.Label>) {
  return (
    <ContextMenuPrimitive.Label
      className={cn("px-2 py-1.5 text-xs font-medium text-muted-foreground", className)}
      {...props}
    />
  );
}

export function ContextMenuSeparator({
  className,
  ...props
}: ComponentProps<typeof ContextMenuPrimitive.Separator>) {
  return (
    <ContextMenuPrimitive.Separator className={cn("-mx-1 my-1 h-px bg-border", className)} {...props} />
  );
}

export function ContextMenuShortcut({ className, ...props }: ComponentProps<"span">) {
  return (
    <span
      className={cn("ml-auto text-xs tracking-widest text-muted-foreground", className)}
      {...props}
    />
  );
}

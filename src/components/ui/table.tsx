/**
 * Table primitives.
 *
 * Deliberately thinner than shadcn's generated `table.tsx`: that one wraps
 * `<table>` in an `overflow-auto` div, which is exactly wrong here. The
 * virtualized `DataTable` owns its own scroll container (the virtualizer needs
 * a stable `scrollElement`) and positions rows absolutely inside a spacer, so a
 * second scroller nested inside would break measurement.
 *
 * `<table>` semantics are kept rather than dropping to divs because the
 * accessibility contract in docs/05-UI.md requires the tables to be readable by
 * VoiceOver. That means the ARIA row/column indices have to be supplied
 * explicitly when rows are virtualized — a screen reader must not be told there
 * are 30 rows when the tree has 4 million.
 */

import type { ComponentProps } from "react";

import { cn } from "@/lib/utils";

export function Table({ className, ...props }: ComponentProps<"table">) {
  return <table className={cn("w-full caption-bottom border-separate border-spacing-0 text-sm", className)} {...props} />;
}

export function TableHeader({ className, ...props }: ComponentProps<"thead">) {
  return <thead className={cn("[&_tr]:border-b", className)} {...props} />;
}

export function TableBody({ className, ...props }: ComponentProps<"tbody">) {
  return <tbody className={cn("relative", className)} {...props} />;
}

export function TableFooter({ className, ...props }: ComponentProps<"tfoot">) {
  return (
    <tfoot
      className={cn("border-t border-border bg-muted/40 font-medium", className)}
      {...props}
    />
  );
}

export function TableRow({ className, ...props }: ComponentProps<"tr">) {
  return (
    <tr
      className={cn(
        "border-b border-border/60 transition-colors",
        "hover:bg-accent/40 data-[state=selected]:bg-brand/15",
        className,
      )}
      {...props}
    />
  );
}

export function TableHead({ className, ...props }: ComponentProps<"th">) {
  return (
    <th
      className={cn(
        "h-8 px-2 text-left align-middle text-xs font-medium tracking-wide text-muted-foreground",
        "bg-background/95 backdrop-blur supports-[backdrop-filter]:bg-background/80",
        "border-b border-border select-none",
        className,
      )}
      {...props}
    />
  );
}

export function TableCell({ className, ...props }: ComponentProps<"td">) {
  return <td className={cn("px-2 py-0 align-middle", className)} {...props} />;
}

export function TableCaption({ className, ...props }: ComponentProps<"caption">) {
  return <caption className={cn("mt-2 text-xs text-muted-foreground", className)} {...props} />;
}

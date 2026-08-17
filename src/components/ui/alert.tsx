/**
 * Alert.
 *
 * docs/05-UI.md, "Scan UX": unreadable directories, exclusions, mutations,
 * aggregation, and a tree-versus-volume discrepancy surface **as alerts on
 * completion**, not as dialogs during the scan — the scanner's policy is that a
 * per-path failure is recorded and the scan continues, so a modal would be
 * lying about severity.
 *
 * `role` is chosen by variant rather than hard-coded to `role="alert"`: an
 * always-present summary region announced assertively would interrupt VoiceOver
 * every time a scan finishes. Only `destructive` is assertive.
 */

import { cva, type VariantProps } from "class-variance-authority";
import type { ComponentProps } from "react";

import { cn } from "@/lib/utils";

const alertVariants = cva(
  [
    "relative grid w-full grid-cols-[auto_1fr] items-start gap-x-3 gap-y-1",
    "rounded-lg border px-4 py-3 text-sm",
    "[&>svg]:size-4 [&>svg]:translate-y-0.5",
  ].join(" "),
  {
    variants: {
      variant: {
        default: "border-border bg-card text-card-foreground [&>svg]:text-muted-foreground",
        info: "border-brand/40 bg-brand/10 text-foreground [&>svg]:text-brand",
        warning:
          "border-pressure-warn/40 bg-pressure-warn/10 text-foreground [&>svg]:text-pressure-warn",
        destructive:
          "border-destructive/50 bg-destructive/10 text-foreground [&>svg]:text-destructive",
      },
    },
    defaultVariants: { variant: "default" },
  },
);

export type AlertProps = ComponentProps<"div"> & VariantProps<typeof alertVariants>;

export function Alert({ className, variant, ...props }: AlertProps) {
  return (
    <div
      role={variant === "destructive" ? "alert" : "status"}
      aria-live={variant === "destructive" ? "assertive" : "polite"}
      className={cn(alertVariants({ variant }), className)}
      {...props}
    />
  );
}

export function AlertTitle({ className, ...props }: ComponentProps<"div">) {
  return (
    <div
      className={cn("col-start-2 font-medium leading-5 tracking-tight", className)}
      {...props}
    />
  );
}

export function AlertDescription({ className, ...props }: ComponentProps<"div">) {
  return (
    <div
      className={cn("col-start-2 text-sm leading-relaxed text-muted-foreground", className)}
      {...props}
    />
  );
}

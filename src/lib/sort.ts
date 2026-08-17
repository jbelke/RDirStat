/**
 * TanStack Table's display sort state -> the backend's `Sort`.
 *
 * The table runs in `manualSorting`, so `SortingState` is not a transform — it
 * is *the request parameter*, plus the arrow drawn in the header. Rust owns
 * ordering because a client-side sort would need the whole tree in JS, which
 * the IPC contract forbids (`children` is capped at 500 rows).
 *
 * A column id with no `SortKey` (the `%Bar`, which is derived, not stored)
 * falls back to the default `Sort::default()` — logical, descending — rather
 * than sending a key the backend would reject.
 */

import type { SortingState } from "@tanstack/react-table";

import type { Sort, SortKey } from "@/lib/bindings";

/** Column id -> the `SortKey` the backend understands. */
export const SORTABLE_COLUMNS: Readonly<Record<string, SortKey>> = {
  name: "name",
  logical: "logical",
  allocated: "allocated",
  category: "category",
  mtime: "mtime",
  kind: "kind",
};

/** `Sort::default()` from the contract: `SortKey::Logical` + `SortDirection::Descending`. */
export const DEFAULT_SORT: Sort = { key: "logical", direction: "descending" };

export function toBackendSort(sorting: SortingState): Sort {
  const first = sorting[0];
  if (first === undefined) return DEFAULT_SORT;
  const key = SORTABLE_COLUMNS[first.id];
  if (key === undefined) return DEFAULT_SORT;
  return { key, direction: first.desc ? "descending" : "ascending" };
}

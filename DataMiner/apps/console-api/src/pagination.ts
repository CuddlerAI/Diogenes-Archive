import { badRequest } from "./errors.js";

export const defaultPageSize = 50;
export const maxPageSize = 200;

export type Cursor = {
  orderedAt: string;
  id: string;
};

// Offset paging drifts whenever a row is inserted mid-scroll and gets slower the
// deeper the operator goes, so every list is keyed on (ordered column, id).
export function encodeCursor(cursor: Cursor): string {
  return Buffer.from(`${cursor.orderedAt}|${cursor.id}`, "utf8").toString("base64url");
}

export function decodeCursor(value: string | undefined): Cursor | null {
  if (!value) return null;
  const decoded = Buffer.from(value, "base64url").toString("utf8");
  const separator = decoded.lastIndexOf("|");
  if (separator <= 0) throw badRequest("invalid_cursor", "the pagination cursor is not valid");
  const orderedAt = decoded.slice(0, separator);
  const id = decoded.slice(separator + 1);
  if (!orderedAt || !id || Number.isNaN(Date.parse(orderedAt))) {
    throw badRequest("invalid_cursor", "the pagination cursor is not valid");
  }
  return { orderedAt, id };
}

export type Page<T> = {
  items: T[];
  nextCursor: string | null;
};

export function toPage<T>(rows: T[], limit: number, cursorOf: (row: T) => Cursor): Page<T> {
  const items = rows.slice(0, limit);
  const last = items.at(-1);
  return {
    items,
    nextCursor: rows.length > limit && last ? encodeCursor(cursorOf(last)) : null,
  };
}

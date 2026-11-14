import { useEffect, useState, type ReactNode } from "react";

import { api } from "./api.js";

export function StateBadge({ label, tone }: { label: string; tone?: "ok" | "warn" | "bad" | "mute" }) {
  return <span className={`badge badge-${tone ?? "mute"}`}>{label}</span>;
}

export function verificationTone(state: string): "ok" | "warn" | "bad" | "mute" {
  if (state === "verified") return "ok";
  if (state === "rejected") return "bad";
  if (state === "queued" || state === "verifying") return "warn";
  return "mute";
}

export function ingestTone(state: string): "ok" | "warn" | "bad" | "mute" {
  if (state === "stored") return "ok";
  if (state === "failed") return "bad";
  if (state === "uploading" || state === "retry_wait") return "warn";
  return "mute";
}

export function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="field">
      <span className="field-label">{label}</span>
      <span className="field-value">{children}</span>
    </div>
  );
}

export function Panel({ title, note, children }: { title: string; note?: string; children: ReactNode }) {
  return (
    <section className="panel">
      <header className="panel-head">
        <h2>{title}</h2>
        {note ? <p className="panel-note">{note}</p> : null}
      </header>
      {children}
    </section>
  );
}

/**
 * Empty is not an error and it is not a placeholder for data that exists
 * elsewhere. Staging carries no annotator, so the reason for an empty list is
 * stated rather than left for the reader to guess.
 */
export function EmptyState({ title, explanation }: { title: string; explanation: string }) {
  return (
    <div className="empty">
      <p className="empty-title">{title}</p>
      <p className="empty-explanation">{explanation}</p>
    </div>
  );
}

export function ErrorNote({ message }: { message: string }) {
  return <p className="error-note">{message}</p>;
}

export function Loading() {
  return <p className="loading">加载中…</p>;
}

export function Mono({ value }: { value: string | null }) {
  return <code className="mono">{value ?? "—"}</code>;
}

export function JsonBlock({ value }: { value: unknown }) {
  return <pre className="json">{JSON.stringify(value, null, 2)}</pre>;
}

/**
 * The annotator list, for the filters and pickers that need to name one.
 *
 * Both the data browser and the device list shipped with a hardcoded
 * `全部` / `未归属` pair, so the "by annotator" dimension the API has always
 * supported could not actually be selected. Loading it in one place keeps the
 * two in step, and failure degrades to the same two options rather than an
 * empty control.
 */
export function useAnnotatorOptions(): { id: string; label: string }[] {
  const [options, setOptions] = useState<{ id: string; label: string }[]>([]);
  useEffect(() => {
    let live = true;
    void api
      .annotators({ pageSize: 200 })
      .then((page) => {
        if (!live) return;
        setOptions(
          page.items.map((item) => ({
            id: item.id,
            // Alias is optional; the short id keeps the row selectable regardless.
            label: item.alias ? `${item.alias}（${item.id.slice(0, 8)}）` : item.id.slice(0, 8),
          })),
        );
      })
      .catch(() => {
        /* Filter stays on 全部/未归属; the page's own error surface covers the rest. */
      });
    return () => {
      live = false;
    };
  }, []);
  return options;
}

import type { ReactNode } from "react";

/**
 * The annotator portal chrome (docs/34 §4.1): brand header + centered column.
 * One place on purpose — /join, /claim-account and /account are the only pages
 * an end user ever sees, and they must read as one product, not three forms.
 */
export function PortalShell({ children }: { children: ReactNode }) {
  return (
    <main className="portal">
      <div className="portal-inner">
        <header className="portal-brand">
          <span className="mark">
            DataMiner<b>·</b>采集员
          </span>
          <small>Diogenes 采集协作门户</small>
        </header>
        {children}
      </div>
    </main>
  );
}

/** Success state: ✓ badge, explanation, one primary action. */
export function PortalDone({
  title,
  message,
  action,
  onAction,
}: {
  title?: string;
  message: ReactNode;
  action: string;
  onAction: () => void;
}) {
  return (
    <div className="portal-done">
      <div className="tick">✓</div>
      {title ? <h2>{title}</h2> : null}
      <p>{message}</p>
      <button type="button" onClick={onAction}>
        {action}
      </button>
    </div>
  );
}

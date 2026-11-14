import {
  createContext,
  useContext,
  useEffect,
  useState,
  type MouseEvent,
  type ReactNode,
} from "react";

/**
 * Minimal history-API router for the authenticated console shell (docs/53 §3).
 * Deliberately not react-router: twelve flat routes, no nesting, no loaders —
 * the whole thing stays reviewable in one screen. The standalone public pages
 * (/join, /account, …) are boot-routed in main.tsx and never pass through here.
 *
 * URL discipline (docs/53 §2): path segments carry enums and server-issued ids
 * only; anything containing slashes (R2 prefixes) rides in the query string;
 * presigned URLs and credentials never enter the URL.
 */

export type RouteLocation = {
  path: string;
  query: URLSearchParams;
};

const RouteContext = createContext<RouteLocation>({
  path: "/",
  query: new URLSearchParams(),
});

const NAVIGATE_EVENT = "console:navigate";

function currentLocation(): RouteLocation {
  return {
    path: window.location.pathname,
    query: new URLSearchParams(window.location.search),
  };
}

export function navigate(to: string, options: { replace?: boolean } = {}): void {
  if (options.replace) window.history.replaceState(null, "", to);
  else window.history.pushState(null, "", to);
  globalThis.dispatchEvent(new CustomEvent(NAVIGATE_EVENT));
}

export function RouterProvider({ children }: { children: ReactNode }) {
  const [location, setLocation] = useState<RouteLocation>(currentLocation);
  useEffect(() => {
    const sync = () => setLocation(currentLocation());
    globalThis.addEventListener("popstate", sync);
    globalThis.addEventListener(NAVIGATE_EVENT, sync);
    return () => {
      globalThis.removeEventListener("popstate", sync);
      globalThis.removeEventListener(NAVIGATE_EVENT, sync);
    };
  }, []);
  return <RouteContext.Provider value={location}>{children}</RouteContext.Provider>;
}

export function useRoute(): RouteLocation {
  return useContext(RouteContext);
}

/** A real anchor so cmd/ctrl/middle-click open a new tab; a plain left click
 * becomes a client-side navigation. */
export function Link({
  to,
  className,
  children,
}: {
  to: string;
  className?: string;
  children: ReactNode;
}) {
  const onClick = (event: MouseEvent<HTMLAnchorElement>) => {
    if (event.defaultPrevented) return;
    if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey)
      return;
    event.preventDefault();
    navigate(to);
  };
  return (
    <a href={to} {...(className ? { className } : {})} onClick={onClick}>
      {children}
    </a>
  );
}

/** Build a path plus encoded query from entries, skipping empty values. */
export function withQuery(path: string, entries: Record<string, string | undefined>): string {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(entries)) {
    if (value !== undefined && value !== "") query.set(key, value);
  }
  const encoded = query.toString();
  return encoded ? `${path}?${encoded}` : path;
}

import { StrictMode, type ReactNode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App.js";
import { RouterProvider } from "./router.js";
import { AccountPortal } from "./pages/AccountPortal.js";
import { ClaimAccount } from "./pages/ClaimAccount.js";
import { Join } from "./pages/Join.js";
import { SetPassword } from "./pages/SetPassword.js";
import "./styles.css";

const container = document.getElementById("root");
if (!container) throw new Error("root element is missing");

// The whole app is one SPA served by an assets-only Worker, so "routing" is just
// a switch on the path the browser landed on. The operator one-time-link pages
// and the annotator-facing pages (docs/34 §4.1: /join self-registration,
// /claim-account for operator-issued set-password links, /account self-service)
// each get a route; everything else is the authenticated console shell, which
// does its own password → session gating inside <App/>. The annotator pages
// share the bundle but never the session: their cookie is __Host-dma_session.
function route(pathname: string): ReactNode {
  if (pathname === "/accept-invite") return <SetPassword mode="invite" />;
  if (pathname === "/reset-password") return <SetPassword mode="reset" />;
  if (pathname === "/join") return <Join />;
  if (pathname === "/claim-account") return <ClaimAccount />;
  if (pathname === "/account") return <AccountPortal />;
  // Everything else is the authenticated console shell, which routes
  // client-side from here on (docs/53): tabs and detail views live in the URL.
  return (
    <RouterProvider>
      <App />
    </RouterProvider>
  );
}

createRoot(container).render(<StrictMode>{route(window.location.pathname)}</StrictMode>);

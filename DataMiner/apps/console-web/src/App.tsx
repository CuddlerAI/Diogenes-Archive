import { useCallback, useEffect, useState } from "react";

import { ApiError, UNAUTHENTICATED_EVENT, api, rememberStepUpFactor, type SessionInfo } from "./api.js";
import { ErrorNote, Loading } from "./components.js";
import { Link, navigate, useRoute, withQuery } from "./router.js";
import { Login } from "./Login.js";
import { Annotators } from "./pages/Annotators.js";
import { Catalog } from "./pages/Catalog.js";
import { DataBrowser, emptyFilters, type DataFilters } from "./pages/DataBrowser.js";
import { Devices } from "./pages/Devices.js";
import { Invites } from "./pages/Invites.js";
import { Operators } from "./pages/Operators.js";
import { Output } from "./pages/Output.js";
import { Overview } from "./pages/Overview.js";
import { Pipeline } from "./pages/Pipeline.js";
import { Review } from "./pages/Review.js";

type Tab =
  | "overview"
  | "output"
  | "pipeline"
  | "annotators"
  | "devices"
  | "data"
  | "review"
  | "catalog"
  | "invites"
  | "operators";

const tabs: { id: Tab; label: string }[] = [
  { id: "overview", label: "总览" },
  { id: "output", label: "产出报表" },
  { id: "pipeline", label: "管道" },
  { id: "annotators", label: "标注用户" },
  { id: "devices", label: "标注设备" },
  { id: "data", label: "数据浏览" },
  { id: "catalog", label: "目录" },
  { id: "invites", label: "邀请码" },
];

const tabLabels: Record<Tab, string> = {
  overview: "总览",
  output: "产出报表",
  pipeline: "管道",
  annotators: "标注用户",
  devices: "标注设备",
  data: "数据浏览",
  review: "数据审查",
  catalog: "目录",
  invites: "邀请码",
  operators: "管理员",
};

const tabIds = new Set<string>(Object.keys(tabLabels));

export function App() {
  const [session, setSession] = useState<SessionInfo | null | "loading">("loading");
  const [notice, setNotice] = useState<string | null>(null);

  // The tab and everything the pages need to restore themselves derive from
  // the URL (docs/53): the first path segment picks the tab, deep segments and
  // the query string belong to the page. Unknown paths land on /overview.
  const { path, query } = useRoute();
  const segment = path.split("/")[1] || "";
  const tab: Tab = tabIds.has(segment) ? (segment as Tab) : "overview";
  useEffect(() => {
    if (!tabIds.has(segment)) navigate("/overview", { replace: true });
  }, [segment]);
  useEffect(() => {
    document.title = `${tabLabels[tab]} · DataMiner Console`;
  }, [tab]);

  const deviceFocus = tab === "devices" ? query.get("focus") : null;
  const filters: DataFilters = {
    gameId: query.get("gameId") ?? "",
    deviceId: query.get("deviceId") ?? "",
    annotatorId: query.get("annotatorId") ?? "",
    verificationState: query.get("verificationState") ?? "",
    ingestState: query.get("ingestState") ?? "",
    unhealthyOnly: query.get("unhealthyOnly") === "1",
  };
  const applyFilters = (next: DataFilters) =>
    navigate(
      withQuery("/data", {
        gameId: next.gameId || undefined,
        deviceId: next.deviceId || undefined,
        annotatorId: next.annotatorId || undefined,
        verificationState: next.verificationState || undefined,
        ingestState: next.ingestState || undefined,
        unhealthyOnly: next.unhealthyOnly ? "1" : undefined,
      }),
      { replace: tab === "data" },
    );

  // Records the step-up factor alongside the session, so the confirmation
  // dialogs know whether a password prompt exists to show.
  const adopt = useCallback((info: SessionInfo) => {
    rememberStepUpFactor(info.stepUp);
    setSession(info);
  }, []);

  // Returns to the sign-in form when any request comes back 401. Guarded on an
  // established session so the sign-in and cf_access bootstrap paths, which
  // legitimately produce 401s before a session exists, are left alone.
  useEffect(() => {
    const onUnauthenticated = () =>
      setSession((current) => (current !== "loading" && current !== null ? null : current));
    globalThis.addEventListener(UNAUTHENTICATED_EVENT, onUnauthenticated);
    return () => globalThis.removeEventListener(UNAUTHENTICATED_EVENT, onUnauthenticated);
  }, []);

  const refresh = useCallback(async () => {
    try {
      adopt(await api.session());
    } catch (thrown) {
      if (thrown instanceof ApiError && thrown.status === 401) {
        // In cf_access mode the console signs in through Cloudflare Access: exchange
        // the edge-injected identity for a session and retry once. Any failure —
        // including the 404 when cf_access is off — falls through to the password form.
        try {
          await api.cfAccessLogin();
          adopt(await api.session());
        } catch {
          setSession(null);
        }
      } else {
        setNotice("无法确认登录状态，请刷新页面。");
      }
    }
  }, [adopt]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  if (session === "loading") return <Loading />;
  if (session === null) return <Login onSignedIn={() => void refresh()} />;

  // Capability comes from the server; hiding a control is only presentation and
  // the API refuses the write regardless.
  // The server does not gate every write on one capability: annotator and device
  // writes require identity.write (governance.ts createAnnotator/updateAnnotator/
  // createDevice/updateDevice/rotateDeviceCredential, invite-admin), while
  // consent, grant and clip writes require governance.write. Gating the whole UI
  // on governance.write happens to work today only because operator and admin
  // hold both — the moment a role holds one and not the other, the buttons would
  // render and the API would refuse them.
  const sudoActive =
    session.session.sudoUntil !== null && Date.parse(session.session.sudoUntil) > Date.now();
  const canWriteGovernance = session.capabilities.includes("governance.write");
  const canWriteIdentity = session.capabilities.includes("identity.write");
  const canPreview = session.capabilities.includes("clip.preview");
  // The data-review surface (docs/33) is operator+ only: frames/events are
  // per-event input streams, a step more sensitive than the video preview.
  const canReview = session.capabilities.includes("review.read");
  // Only an admin gets the operator-management tab. Hiding it is presentation
  // only; the /operators endpoints refuse every non-admin regardless.
  const canManageOperators = session.capabilities.includes("operator.admin");
  let visibleTabs = tabs;
  if (canReview) {
    const dataIndex = visibleTabs.findIndex((entry) => entry.id === "data");
    visibleTabs = [
      ...visibleTabs.slice(0, dataIndex + 1),
      { id: "review" as const, label: "数据审查" },
      ...visibleTabs.slice(dataIndex + 1),
    ];
  }
  if (canManageOperators) {
    visibleTabs = [...visibleTabs, { id: "operators" as const, label: "管理员" }];
  }

  async function signOut() {
    try {
      await api.logout();
    } finally {
      setSession(null);
    }
  }

  return (
    <div className="shell">
      <header className="topbar">
        <div className="brand">
          DataMiner Console
          {/* Never hardcode this: a production console that says "staging" is
              the one label an operator must be able to trust before acting on
              real data. "unknown" when the API predates the field — wrong is
              worse than absent here. */}
          <span className="env">{session.environment ?? "unknown"}</span>
        </div>
        <nav>
          {visibleTabs.map((entry) => (
            <Link
              key={entry.id}
              to={`/${entry.id}`}
              className={tab === entry.id ? "nav nav-active" : "nav"}
            >
              {entry.label}
            </Link>
          ))}
        </nav>
        <div className="who">
          <span>{session.operator.email}</span>
          <span className="role">{session.operator.role}</span>
          <button className="link" onClick={() => void signOut()}>
            退出
          </button>
        </div>
      </header>

      {notice ? <ErrorNote message={notice} /> : null}

      <main className="content">
        {tab === "catalog" ? <Catalog /> : null}
        {tab === "invites" ? (
          <Invites
            canWrite={canWriteIdentity}
            sudoActive={sudoActive}
            onElevated={() => void refresh()}
          />
        ) : null}
        {tab === "operators" && canManageOperators ? (
          <Operators
            me={session.operator}
            sudoActive={sudoActive}
            onElevated={() => void refresh()}
          />
        ) : null}
        {tab === "overview" ? <Overview /> : null}
        {tab === "output" ? <Output /> : null}
        {tab === "pipeline" ? <Pipeline /> : null}
        {tab === "annotators" ? (
          <Annotators
            canWrite={canWriteIdentity}
            onOpenDevice={(deviceId) => navigate(withQuery("/devices", { focus: deviceId }))}
          />
        ) : null}
        {tab === "devices" ? (
          <Devices
            canWrite={canWriteIdentity}
            focusId={deviceFocus}
            onFocus={(deviceId) =>
              navigate(withQuery("/devices", { focus: deviceId ?? undefined }), { replace: true })
            }
            onBrowseData={(deviceId) => applyFilters({ ...emptyFilters, deviceId })}
          />
        ) : null}
        {tab === "data" ? (
          <DataBrowser
            filters={filters}
            onFiltersChange={applyFilters}
            canWrite={canWriteGovernance}
            canPreview={canPreview}
          />
        ) : null}
        {tab === "review" && canReview ? <Review canWrite={canWriteGovernance} /> : null}
      </main>

      {/* docs/65 J-49：此前写死「M2a staging：数据固定为 non-trainable」，生产也照显。
          措辞保持不主张分类——可训练性是服务端逐 clip 派生的，界面不复述它。 */}
      <footer className="footer">
        可训练性由数据库 fail-closed 视图逐 clip 派生；车道由目录渠道决定，界面不改写分类。
      </footer>
    </div>
  );
}

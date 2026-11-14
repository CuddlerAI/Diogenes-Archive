import { useState } from "react";

import { api } from "../api.js";
import {
  EmptyState,
  ErrorNote,
  Field,
  JsonBlock,
  Loading,
  Mono,
  Panel,
  StateBadge,
  useAnnotatorOptions,
} from "../components.js";
import { formatBytes, formatDateTime } from "../format.js";
import { useAsync } from "../useAsync.js";
import { AuditTrail } from "./AuditTrail.js";
import { ClientEventsPanel } from "./ClientEvents.js";
import { ContributionPanel } from "./Contribution.js";
import { ConfirmedAction, IssuedToken, RegisterDevice } from "./Governance.js";

export function Devices({
  focusId,
  onFocus,
  onBrowseData,
  canWrite,
}: {
  focusId: string | null;
  onFocus: (id: string | null) => void;
  onBrowseData: (deviceId: string) => void;
  canWrite: boolean;
}) {
  const annotatorOptions = useAnnotatorOptions();
  const [status, setStatus] = useState("");
  const [attachment, setAttachment] = useState("");
  const [query, setQuery] = useState("");
  const [revision, setRevision] = useState(0);
  const list = useAsync(
    () =>
      api.devices({
        status: status || undefined,
        annotatorId: attachment || undefined,
        query: query || undefined,
        pageSize: 50,
      }),
    [status, attachment, query, revision],
  );
  if (focusId) {
    return (
      <DeviceDetailView
        id={focusId}
        canWrite={canWrite}
        onBack={() => onFocus(null)}
        onBrowseData={onBrowseData}
      />
    );
  }

  return (
    <Panel title="标注设备" note="设备参数为客户端登记时提交的快照，云端不独立复核硬件。">
      <div className="filters">
        <label>
          状态
          <select value={status} onChange={(event) => setStatus(event.target.value)}>
            <option value="">全部</option>
            <option value="pending">pending</option>
            <option value="active">active</option>
            <option value="suspended">suspended</option>
            <option value="retired">retired</option>
          </select>
        </label>
        <label>
          归属
          <select value={attachment} onChange={(event) => setAttachment(event.target.value)}>
            <option value="">全部</option>
            <option value="none">未归属标注用户</option>
            {annotatorOptions.map((option) => (
              <option key={option.id} value={option.id}>
                {option.label}
              </option>
            ))}
          </select>
        </label>
        <label>
          设备 ID
          <input value={query} onChange={(event) => setQuery(event.target.value)} placeholder="搜索 client_device_id" />
        </label>
      </div>

      {canWrite ? (
        <RegisterDevice onRegistered={() => setRevision((current) => current + 1)} />
      ) : null}

      {list.status === "loading" ? <Loading /> : null}
      {list.status === "error" ? <ErrorNote message={list.message} /> : null}
      {list.status === "ready" && list.value.items.length === 0 ? (
        <EmptyState
          title="没有设备"
          explanation="设备行目前需要带外登记（credential_fingerprint = sha256(token)）。可用本页的「登记设备」表单建档。"
        />
      ) : null}

      {list.status === "ready" && list.value.items.length > 0 ? (
        <table>
          <thead>
            <tr>
              <th>设备 ID</th>
              <th>归属</th>
              <th>状态</th>
              <th>系统</th>
              <th>GPU 驱动</th>
              <th>客户端</th>
              <th>会话</th>
              <th>片段</th>
              <th>存储量</th>
              <th>最近上报</th>
            </tr>
          </thead>
          <tbody>
            {list.value.items.map((device) => (
              <tr key={device.id} className="clickable" onClick={() => onFocus(device.id)}>
                <td>{device.clientDeviceId}</td>
                <td>{device.annotatorAlias ?? <span className="muted">未归属</span>}</td>
                <td>
                  <StateBadge label={device.status} tone={device.status === "active" ? "ok" : "warn"} />
                </td>
                <td>{device.windowsBuild}</td>
                <td>{device.gpuDriver}</td>
                <td>{device.clientVersion}</td>
                <td>{device.sessionCount}</td>
                <td>{device.clipCount}</td>
                <td>{formatBytes(device.storedBytes)}</td>
                <td>{formatDateTime(device.lastSeenAt)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : null}
    </Panel>
  );
}

function DeviceDetailView({
  id,
  canWrite,
  onBack,
  onBrowseData,
}: {
  id: string;
  canWrite: boolean;
  onBack: () => void;
  onBrowseData: (deviceId: string) => void;
}) {
  const annotatorOptions = useAnnotatorOptions();
  const [revision, setRevision] = useState(0);
  const state = useAsync(() => api.device(id), [id, revision]);
  const [showFingerprint, setShowFingerprint] = useState(false);

  if (state.status === "loading") return <Loading />;
  if (state.status === "error") return <ErrorNote message={state.message} />;
  const device = state.value;

  return (
    <>
      <button className="link" onClick={onBack}>
        ← 返回列表
      </button>

      <Panel title={device.clientDeviceId}>
        <div className="fields">
          <Field label="状态">
            <StateBadge label={device.status} tone={device.status === "active" ? "ok" : "warn"} />
          </Field>
          <Field label="归属标注用户">{device.annotatorAlias ?? "未归属"}</Field>
          <Field label="系统版本">{device.windowsBuild}</Field>
          <Field label="GPU">{device.gpuPciId}</Field>
          <Field label="GPU 驱动">{device.gpuDriver}</Field>
          <Field label="客户端版本">{device.clientVersion}</Field>
          <Field label="最近上报">{formatDateTime(device.lastSeenAt)}</Field>
          <Field label="登记时间">{formatDateTime(device.createdAt)}</Field>
          <Field label="凭据指纹">
            {showFingerprint ? (
              <Mono value={device.credentialFingerprint} />
            ) : (
              <>
                <Mono value={`${device.credentialFingerprint.slice(0, 8)}…`} />{" "}
                <button className="link" onClick={() => setShowFingerprint(true)}>
                  显示完整值
                </button>
              </>
            )}
          </Field>
          {device.certifiedProfileId ? (
            <Field label="历史设备档案">
              <Mono value={device.certifiedProfileId} />
            </Field>
          ) : null}
        </div>
        <p className="muted">
          指纹是设备 token 的 sha256，本身不是秘密；token 明文只在签发时展示一次，云端不保存。
        </p>
        {device.certifiedProfileId ? (
          <p className="muted">历史登记快照，不参与录制或上传准入。</p>
        ) : null}
        <button className="link" onClick={() => onBrowseData(device.id)}>
          在数据浏览中查看该设备的会话 →
        </button>
        {canWrite ? (
          <div className="action-row">
            {/* Until this existed there was no way to attach a device to an
                annotator from the console, so every console-registered device
                stayed unattached and the "by annotator" filters could never
                match one — the API has accepted annotatorId all along. */}
            <ConfirmedAction
              label={device.annotatorAlias ? "改归属" : "归属到标注用户"}
              title="设置设备归属"
              confirmation="归属决定这台设备的数据算在哪个标注用户名下。留空表示解除归属。"
              fields={[
                {
                  name: "annotatorId",
                  label: "标注用户 ID（留空解除归属）",
                  placeholder: annotatorOptions[0]?.id ?? "annotator uuid",
                },
              ]}
              run={async (values, key) => {
                const raw = values.annotatorId?.trim() ?? "";
                await api.updateDevice(id, { annotatorId: raw === "" ? "none" : raw }, key);
              }}
              onDone={() => setRevision((current) => current + 1)}
            />
            <ConfirmedAction
              label={device.status === "active" ? "暂停设备" : "启用设备"}
              title={device.status === "active" ? "暂停设备" : "启用设备"}
              confirmation={
                device.status === "active"
                  ? "暂停后该设备的凭据仍然存在，但不应再被视为可用身份。"
                  : "启用后该设备重新成为可用身份。"
              }
              destructive={device.status === "active"}
              run={async (_values, key) => {
                await api.updateDevice(
                  id,
                  { status: device.status === "active" ? "suspended" : "active" },
                  key,
                );
              }}
              onDone={() => setRevision((current) => current + 1)}
            />
            <ConfirmedAction
              label="轮换凭据"
              title="轮换设备凭据"
              confirmation="轮换会立即使旧 token 失效，持有旧 token 的客户端将无法再登记数据。新 token 只显示一次。"
              destructive
              run={async (_values, key) => {
                const issued = await api.rotateDeviceCredential(id, key);
                return <IssuedToken token={issued.token} />;
              }}
              onDone={() => setRevision((current) => current + 1)}
            />
          </div>
        ) : null}
      </Panel>

      <Panel title="显示与输入快照" note="客户端登记时提交的原始快照，未经云端复核。">
        <div className="split">
          <div>
            <h3>显示</h3>
            <JsonBlock value={device.displaySnapshot} />
          </div>
          <div>
            <h3>输入设备</h3>
            <JsonBlock value={device.inputDevicesSnapshot} />
          </div>
        </div>
      </Panel>

      <ContributionPanel load={(days, timeBasis) => api.deviceContribution(id, days, timeBasis)} />

      <ClientEventsPanel title="客户端遥测" params={{ deviceId: id }} />

      <AuditTrail
        title="行为日志"
        note="口径为服务端审计事实。客户端本地 JSONL 诊断不上传；经白名单的客户端遥测在下方「客户端遥测」面板。"
        // Without actorId this lists every device's events, so device A's page
        // showed device B's uploads and seals — the attribution is the whole
        // point of the panel. The ingest side writes actor_id = deviceId.
        params={{ actorType: "device", actorId: id }}
      />
    </>
  );
}

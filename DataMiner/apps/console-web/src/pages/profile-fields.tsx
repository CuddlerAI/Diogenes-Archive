import type { AnnotatorProfile, ProfileInput } from "../api.js";

/**
 * The contact-profile form fragment (docs/34 §5.4), shared by self-service
 * registration, the account page, and the operator-side forms so the five
 * fields cannot drift apart. Everything optional — the server treats a missing
 * field as cleared, which is also how a stale phone number gets removed.
 *
 * Renders a two-column `.field-grid` (single column on narrow screens). The
 * optional legend becomes a `.form-sec` section header so the block reads the
 * same in the portal and in the console panel that reuses it.
 */

export const IM_KINDS = [
  { value: "", label: "不填" },
  { value: "wechat", label: "微信" },
  { value: "qq", label: "QQ" },
  { value: "telegram", label: "Telegram" },
  { value: "discord", label: "Discord" },
  { value: "other", label: "其他" },
] as const;

export function ProfileFields({
  values,
  onChange,
  legend = "联络资料",
  note = "全部可选，仅用于运营协调采集",
}: {
  values: Record<string, string>;
  onChange: (values: Record<string, string>) => void;
  legend?: string;
  note?: string;
}) {
  function set(name: string, value: string) {
    onChange({ ...values, [name]: value });
  }

  return (
    <>
      {legend ? (
        <div className="form-sec">
          <strong>{legend}</strong>
          {note ? <small>{note}</small> : null}
        </div>
      ) : null}
      <div className="field-grid">
        <label>
          称呼
          <input
            value={values.displayName ?? ""}
            maxLength={80}
            onChange={(event) => set("displayName", event.target.value)}
          />
        </label>
        <label>
          电话
          <input
            value={values.phone ?? ""}
            maxLength={32}
            onChange={(event) => set("phone", event.target.value)}
          />
        </label>
        <label>
          IM 渠道
          <select value={values.imKind ?? ""} onChange={(event) => set("imKind", event.target.value)}>
            {IM_KINDS.map((kind) => (
              <option key={kind.value} value={kind.value}>
                {kind.label}
              </option>
            ))}
          </select>
        </label>
        <label>
          IM 帐号
          <input
            value={values.imHandle ?? ""}
            maxLength={128}
            onChange={(event) => set("imHandle", event.target.value)}
            placeholder={values.imKind ? "" : "先选择渠道"}
            disabled={!values.imKind}
          />
        </label>
        <label className="span-2">
          所在地区
          <input
            value={values.region ?? ""}
            maxLength={64}
            onChange={(event) => set("region", event.target.value)}
          />
        </label>
      </div>
    </>
  );
}

/**
 * Collapses the form values into the PUT/POST shape. Returns undefined when
 * every field is blank so callers can omit the object entirely; an IM kind
 * without a handle (or vice versa) is dropped rather than sent, matching the
 * server's paired-or-absent constraint.
 */
export function profileFromValues(values: Record<string, string>): ProfileInput | undefined {
  const trimmed = {
    displayName: values.displayName?.trim() ?? "",
    phone: values.phone?.trim() ?? "",
    imKind: values.imKind?.trim() ?? "",
    imHandle: values.imHandle?.trim() ?? "",
    region: values.region?.trim() ?? "",
  };
  const imComplete = trimmed.imKind !== "" && trimmed.imHandle !== "";
  const profile: ProfileInput = {
    ...(trimmed.displayName ? { displayName: trimmed.displayName } : {}),
    ...(trimmed.phone ? { phone: trimmed.phone } : {}),
    ...(imComplete
      ? { imKind: trimmed.imKind as NonNullable<ProfileInput["imKind"]>, imHandle: trimmed.imHandle }
      : {}),
    ...(trimmed.region ? { region: trimmed.region } : {}),
  };
  return Object.keys(profile).length > 0 ? profile : undefined;
}

/** Prefills the form values from a stored profile row. */
export function valuesFromProfile(profile: AnnotatorProfile | null): Record<string, string> {
  if (!profile) return {};
  return {
    displayName: profile.displayName ?? "",
    phone: profile.phone ?? "",
    imKind: profile.imKind ?? "",
    imHandle: profile.imHandle ?? "",
    region: profile.region ?? "",
  };
}

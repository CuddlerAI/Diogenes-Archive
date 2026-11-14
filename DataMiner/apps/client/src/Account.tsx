import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

import { LogoMark } from "./brand";
import { ShieldIcon } from "./icons.generated";
import {
  accountEnvironmentBadge,
  accountReadyLead,
  accountStatusReadCopy,
  accountUnavailableCopy,
  deviceEnrollmentStateCopy,
  deviceEnrollmentSuccessCopy,
  type AccountEnvironment,
} from "./account-presentation";
import {
  consentScopeLabel,
  isAccountError,
  type AccountProfile,
  type AccountStatus,
  type ConsentState,
  type DeviceEnrollmentStatus,
  type EnrollOutcome,
  type ProfileUpdate,
  type SignupChallenge,
  type Terms,
} from "./account-contract";

/**
 * 标注员账号（docs/11 §7）：邀请码注册 → 邮箱验证 → 登录 → 登记本机。
 *
 * 账号 UI 不直接控制采集状态，也不改写已有本地 artifact；新采集仍由所选账号环境的
 * 验签目录与采集页 preflight 决定。PRODUCTION 登录与登记会影响上传资格；STAGING 只验证
 * 账号与登记，上传环境不在本次验收范围。
 *
 * 密码只存在于表单的局部 state，提交后立即清空：不进 App 顶层 state，不进
 * localStorage，也不随任何事件外发。
 */

type Step = "overview" | "signIn" | "signUpForm" | "signUpVerify" | "resetRequest" | "resetConfirm";

/** 注册路径的三步。登录既是这条路径的终点，也是老用户的入口——两种身份共用同一个
 *  step，靠 `inSignUpFlow` 决定要不要画进度条。 */
const SIGNUP_STEPS: { key: Step; label: string }[] = [
  { key: "signUpForm", label: "填写资料" },
  { key: "signUpVerify", label: "验证邮箱" },
  { key: "signIn", label: "登录" },
];

/** 重发冷却。服务端也会限流，这里的意义是让按钮自己说出「还要等多久」，
 *  而不是让人反复点击换来一个同形的响应。 */
const RESEND_COOLDOWN_SECONDS = 60;

function errorMessage(thrown: unknown, fallback: string): string {
  if (!isAccountError(thrown)) return fallback;
  return thrown.requestId
    ? `${thrown.message}（错误编号：${thrown.requestId}）`
    : thrown.message;
}

/** a***@example.com —— 验证步骤要能确认「码发到哪儿了」，又不必把整个地址
 *  重新摆在屏幕上。 */
function maskEmail(value: string): string {
  const at = value.indexOf("@");
  if (at <= 0) return value;
  const name = value.slice(0, at);
  return `${name.slice(0, 1)}${"*".repeat(Math.max(1, Math.min(name.length - 1, 3)))}${value.slice(at)}`;
}

export function AccountWorkspace({
  status,
  onStatusChange,
}: {
  status: AccountStatus;
  onStatusChange: (next: AccountStatus) => void;
}) {
  const [step, setStep] = useState<Step>("overview");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [terms, setTerms] = useState<Terms | null>(null);
  const [consentState, setConsentState] = useState<ConsentState | null>(null);
  const [consentAccepted, setConsentAccepted] = useState(false);
  // 条款取不到时，界面必须承认这件事。此前失败后仍停在「正在获取当前条款…」——
  // 一句永远不会兑现的进行时，配上一个永远点不动的注册按钮，没有重试的出口。
  const [termsStatus, setTermsStatus] = useState<"idle" | "loading" | "loaded" | "failed">("idle");
  const [challenge, setChallenge] = useState<SignupChallenge | null>(null);
  const [cooldown, setCooldown] = useState(0);
  // 只有真的走过注册路径，才显示那条三步进度。直接来登录的人没做过前两步，
  // 把它们画成「已完成」是在描述一段没发生的历史。
  const [inSignUpFlow, setInSignUpFlow] = useState(false);

  const [deviceStatus, setDeviceStatus] = useState<DeviceEnrollmentStatus | null>(null);
  const [deviceBusy, setDeviceBusy] = useState(false);

  // 注册表单
  const [inviteCode, setInviteCode] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [alias, setAlias] = useState("");
  const [accepted, setAccepted] = useState(false);
  const [verificationCode, setVerificationCode] = useState("");

  const firstField = useRef<HTMLInputElement | null>(null);

  const clearSecrets = useCallback(() => {
    setPassword("");
    setInviteCode("");
    setVerificationCode("");
  }, []);

  /** 换步骤时把上一步的横幅一起带走：留在屏幕上的旧报错会被当成新报错。 */
  const goto = useCallback((next: Step) => {
    setStep(next);
    setError(null);
    // 用户主动换页时，上一步的提示就过期了——否则「密码已重置，请用新密码登录」
    // 会跟着他到重置表单上，读起来像是又重置了一次。成功路径用的是直接 setStep，
    // 它们设的提示照旧保留。
    setNotice(null);
  }, []);

  const loadTerms = useCallback(async () => {
    setError(null);
    setTermsStatus("loading");
    try {
      setTerms(await invoke<Terms>("account_get_terms"));
      setTermsStatus("loaded");
    } catch (thrown) {
      setTermsStatus("failed");
      setError(errorMessage(thrown, "无法获取当前条款，请稍后重试。"));
    }
  }, []);

  const loadConsentState = useCallback(async () => {
    try {
      setConsentState(await invoke<ConsentState>("account_get_consent_state"));
    } catch {
      // 取不到不该拦住这一页的其它内容；补签区块整体不显示。
      setConsentState(null);
    }
  }, []);

  const loadDeviceStatus = useCallback(async () => {
    setDeviceBusy(true);
    try {
      const next = await invoke<DeviceEnrollmentStatus>("account_get_device_enrollment_status");
      setDeviceStatus(next);
    } catch (thrown) {
      setError(errorMessage(thrown, "无法读取设备登记状态，请稍后重试。"));
    } finally {
      setDeviceBusy(false);
    }
  }, []);

  useEffect(() => {
    // 注册要读条款；补签同样要——对一份读不到的文书表示同意，不算同意。
    const needsTerms =
      step === "signUpForm" ||
      (step === "overview" && consentState !== null && !consentState.matchesCurrentTerms);
    if (!needsTerms) {
      // 离开表单就把失败态丢掉：再进来是一次新的尝试，不该继承上一次的失败。
      // 已经取到的条款留着——它对这一版政策是常量，重取只是多一次请求。
      if (termsStatus === "failed") setTermsStatus("idle");
      return;
    }
    if (termsStatus === "idle") void loadTerms();
  }, [step, termsStatus, loadTerms, consentState]);

  useEffect(() => {
    if (step === "overview" && status.identity.kind === "signedIn") {
      void loadConsentState();
    } else if (status.identity.kind !== "signedIn") {
      setConsentState(null);
    }
  }, [step, status.identity.kind, loadConsentState]);

  useEffect(() => {
    if (step === "overview" && status.identity.kind === "signedIn") {
      void loadDeviceStatus();
    } else if (status.identity.kind !== "signedIn") {
      setDeviceStatus(null);
    }
  }, [step, status.identity.kind, loadDeviceStatus]);

  // 每步进来就把焦点放在第一个输入框：这是个键盘流程，不该先要一次点击。
  useEffect(() => {
    firstField.current?.focus();
  }, [step]);

  useEffect(() => {
    if (cooldown <= 0) return;
    const timer = window.setTimeout(() => setCooldown((value) => value - 1), 1_000);
    return () => window.clearTimeout(timer);
  }, [cooldown]);

  const unavailableCopy = accountUnavailableCopy(status);
  if (unavailableCopy) {
    return (
      <section className="account-view">
        <div className="auth">
          <header className="auth-head">
            <LogoMark />
            <span className="section-kicker">ANNOTATOR ACCOUNT</span>
            <h2>{unavailableCopy.title}</h2>
            <p>{unavailableCopy.body}</p>
          </header>
        </div>
      </section>
    );
  }
  if (status.availability.kind !== "ready") {
    throw new Error("account unavailable state did not produce presentation copy");
  }
  const accountEnvironment = status.availability.environment;
  const environmentBadge = accountEnvironmentBadge(status);

  async function signUp() {
    setBusy(true);
    setError(null);
    try {
      const issued = await invoke<SignupChallenge>("account_signup", {
        request: {
          inviteCode: inviteCode.trim(),
          email: email.trim(),
          password,
          alias: alias.trim(),
          acceptedPolicyVersion: terms?.policyVersion ?? "",
        },
      });
      setChallenge(issued);
      setPassword("");
      setStep("signUpVerify");
      setCooldown(RESEND_COOLDOWN_SECONDS);
      setNotice("验证码已发送到你的邮箱，十分钟内有效。");
    } catch (thrown) {
      setError(errorMessage(thrown, "注册失败，请稍后重试。"));
    } finally {
      setBusy(false);
    }
  }

  async function verify() {
    if (!challenge) return;
    setBusy(true);
    setError(null);
    try {
      await invoke("account_verify_signup", {
        request: { challengeId: challenge.challengeId, code: verificationCode.trim() },
      });
      clearSecrets();
      setChallenge(null);
      setStep("signIn");
      setNotice("邮箱已验证，现在可以登录了。");
    } catch (thrown) {
      setError(errorMessage(thrown, "验证失败，请检查验证码。"));
    } finally {
      setBusy(false);
    }
  }

  async function resend() {
    if (!challenge || cooldown > 0) return;
    setBusy(true);
    setError(null);
    try {
      // 服务端对未知/已验证的 challenge 也返回同形响应，所以这里的成功提示
      // 不代表真的发出了信——这是刻意的，它不该变成一个「这个账号存在吗」的探针。
      setChallenge(
        await invoke<SignupChallenge>("account_resend_signup_code", {
          request: { challengeId: challenge.challengeId },
        }),
      );
      setCooldown(RESEND_COOLDOWN_SECONDS);
      setNotice("如果这个账号还需要验证，新的验证码已经发出。");
    } catch (thrown) {
      setError(errorMessage(thrown, "重发失败，请稍后重试。"));
    } finally {
      setBusy(false);
    }
  }

  async function requestReset() {
    setBusy(true);
    setError(null);
    try {
      const issued = await invoke<SignupChallenge>("account_request_password_reset", {
        request: { email: email.trim() },
      });
      setChallenge(issued);
      setVerificationCode("");
      setPassword("");
      setStep("resetConfirm");
      setCooldown(RESEND_COOLDOWN_SECONDS);
      // 服务端对「有账号」「没账号」返回同一形状，所以这句必须是条件式的：
      // 说成「验证码已发送」就把这个界面变成了查号器。
      setNotice("如果这个邮箱有账号，验证码已经发出，十分钟内有效。");
    } catch (thrown) {
      setError(errorMessage(thrown, "发送失败，请稍后重试。"));
    } finally {
      setBusy(false);
    }
  }

  async function confirmReset() {
    if (!challenge) return;
    setBusy(true);
    setError(null);
    try {
      await invoke("account_confirm_password_reset", {
        request: {
          challengeId: challenge.challengeId,
          code: verificationCode.trim(),
          password,
        },
      });
      clearSecrets();
      setChallenge(null);
      setStep("signIn");
      // 重置会吊销全部会话，所以这里一定是回登录页而不是直接进已登录态。
      setNotice("密码已重置，请用新密码登录。");
    } catch (thrown) {
      setError(errorMessage(thrown, "重置失败，请检查验证码是否正确或已过期。"));
    } finally {
      setBusy(false);
    }
  }

  async function signIn() {
    setBusy(true);
    setError(null);
    try {
      const next = await invoke<AccountStatus>("account_login", {
        request: { email: email.trim(), password },
      });
      clearSecrets();
      onStatusChange(next);
      setInSignUpFlow(false);
      setStep("overview");
      setNotice(null);
    } catch (thrown) {
      setError(errorMessage(thrown, "登录失败，请稍后重试。"));
    } finally {
      setBusy(false);
    }
  }

  async function renewConsent() {
    if (!terms) return;
    setBusy(true);
    setError(null);
    try {
      await invoke("account_renew_consent", {
        request: { acceptedPolicyVersion: terms.policyVersion },
      });
      setConsentAccepted(false);
      await loadConsentState();
      // 主界面的横幅读的是观察器快照，不是这一页的状态。不在这里刷新它，
      // 签完之后横幅会一直挂到下一次 6 小时轮询——一条已经不真的提示。
      await invoke("account_refresh_consent_notice").catch(() => {});
      setNotice("已签署当前条款。");
    } catch (thrown) {
      setError(errorMessage(thrown, "签署失败，请稍后重试。"));
    } finally {
      setBusy(false);
    }
  }

  async function signOut() {
    setBusy(true);
    try {
      onStatusChange(await invoke<AccountStatus>("account_logout"));
      setStep("overview");
    } catch (thrown) {
      setError(errorMessage(thrown, "退出失败，请稍后重试。"));
    } finally {
      setBusy(false);
    }
  }

  async function enrollDevice() {
    if (status.availability.kind !== "ready") {
      setError("账号服务当前不可用，无法登记设备。");
      return;
    }
    setDeviceBusy(true);
    setError(null);
    try {
      const outcome = await invoke<EnrollOutcome>("account_enroll_device");
      setNotice(
        deviceEnrollmentSuccessCopy(status.availability.environment, outcome.credentialIssued),
      );
      await loadDeviceStatus();
    } catch (thrown) {
      setError(errorMessage(thrown, "设备登记失败，请稍后重试。"));
    } finally {
      setDeviceBusy(false);
    }
  }

  /** 离开注册表单：清掉机密，并把勾选状态一起复位——重新进来时应当重新同意，
   *  而不是继承上一次的勾。 */
  function abandonSignUp() {
    clearSecrets();
    setAccepted(false);
    setInSignUpFlow(false);
    goto("overview");
  }

  const headline =
    step === "signIn"
      ? { title: "登录", lead: accountReadyLead(accountEnvironment, "signIn") }
      : step === "signUpForm"
        ? { title: "用邀请码注册", lead: "DataMiner 目前只对持邀请码的标注员开放。" }
        : step === "signUpVerify"
          ? { title: "验证邮箱", lead: `验证码已发往 ${maskEmail(email)}，十分钟内有效。` }
          : step === "resetRequest"
            ? { title: "重置密码", lead: "填写注册时使用的邮箱，我们会发送一个验证码。" }
            : step === "resetConfirm"
              ? {
                  title: "设置新密码",
                  lead: `如果 ${maskEmail(email)} 有账号，验证码已经发出，十分钟内有效。`,
                }
              : null;

  return (
    <section className="account-view">
      <div className="auth">
        <header className="auth-head">
          <LogoMark />
          <span className="section-kicker">ANNOTATOR ACCOUNT</span>
          {environmentBadge ? (
            <span
              className={`device-badge device-${environmentBadge.tone}`}
              aria-label={environmentBadge.accessibleLabel}
              title={environmentBadge.accessibleLabel}
            >
              {environmentBadge.label}
            </span>
          ) : null}
          <h2>{headline?.title ?? "标注员账号"}</h2>
          <p>{headline?.lead ?? accountReadyLead(accountEnvironment, "overview")}</p>
        </header>

        {inSignUpFlow && SIGNUP_STEPS.some((s) => s.key === step) ? (
          <SignUpProgress current={step} />
        ) : null}

        {error ? (
          <div className="auth-banner error" role="alert">
            <div>
              <strong>无法继续</strong>
              <p>{error}</p>
            </div>
          </div>
        ) : null}
        {notice && !error ? (
          <div className="auth-banner notice" role="status">
            <div>
              <p>{notice}</p>
            </div>
          </div>
        ) : null}

        {step === "overview" ? (
          <AccountOverview
            status={status}
            environment={accountEnvironment}
            busy={busy}
            deviceBusy={deviceBusy}
            deviceStatus={deviceStatus}
            onEnrollDevice={() => void enrollDevice()}
            onReloadDevices={() => void loadDeviceStatus()}
            onSignIn={() => goto("signIn")}
            onSignUp={() => {
              setInSignUpFlow(true);
              goto("signUpForm");
            }}
            onSignOut={() => void signOut()}
            consentState={consentState}
            terms={terms}
            consentAccepted={consentAccepted}
            onConsentAcceptedChange={setConsentAccepted}
            onRenewConsent={() => void renewConsent()}
          />
        ) : null}

        {step === "signIn" ? (
          <form
            className="auth-form"
            onSubmit={(event) => {
              event.preventDefault();
              void signIn();
            }}
          >
            <label className="field">
              <span>邮箱</span>
              <input
                ref={firstField}
                type="email"
                autoComplete="username"
                placeholder="you@example.com"
                value={email}
                disabled={busy}
                onChange={(event) => setEmail(event.target.value)}
                required
              />
            </label>
            <label className="field">
              <span>密码</span>
              <input
                type="password"
                autoComplete="current-password"
                value={password}
                disabled={busy}
                onChange={(event) => setPassword(event.target.value)}
                required
              />
            </label>
            <button className="button primary auth-submit" type="submit" disabled={busy}>
              {busy ? "登录中…" : "登录"}
            </button>
            <div className="auth-alt">
              <span>还没有账号？</span>
              <button
                type="button"
                className="link-button"
                disabled={busy}
                onClick={() => {
                  setInSignUpFlow(true);
                  goto("signUpForm");
                }}
              >
                用邀请码注册
              </button>
            </div>
            <div className="auth-alt">
              <span>忘记密码？</span>
              <button
                type="button"
                className="link-button"
                disabled={busy}
                onClick={() => {
                  setInSignUpFlow(false);
                  goto("resetRequest");
                }}
              >
                用邮箱重置
              </button>
            </div>
          </form>
        ) : null}

        {step === "resetRequest" ? (
          <form
            className="auth-form"
            onSubmit={(event) => {
              event.preventDefault();
              void requestReset();
            }}
          >
            <label className="field">
              <span>邮箱</span>
              <input
                ref={firstField}
                type="email"
                autoComplete="username"
                placeholder="you@example.com"
                value={email}
                disabled={busy}
                onChange={(event) => setEmail(event.target.value)}
                required
              />
            </label>
            <button className="button primary auth-submit" type="submit" disabled={busy}>
              {busy ? "发送中…" : "发送验证码"}
            </button>
            <div className="auth-alt">
              <button
                type="button"
                className="link-button"
                disabled={busy}
                onClick={() => goto("signIn")}
              >
                返回登录
              </button>
            </div>
          </form>
        ) : null}

        {step === "resetConfirm" ? (
          <form
            className="auth-form"
            onSubmit={(event) => {
              event.preventDefault();
              void confirmReset();
            }}
          >
            <label className="field">
              <span>验证码</span>
              <input
                ref={firstField}
                inputMode="numeric"
                autoComplete="one-time-code"
                placeholder="6 位数字"
                value={verificationCode}
                disabled={busy}
                onChange={(event) => setVerificationCode(event.target.value)}
                required
              />
            </label>
            <label className="field">
              <span>新密码</span>
              <input
                type="password"
                autoComplete="new-password"
                value={password}
                disabled={busy}
                onChange={(event) => setPassword(event.target.value)}
                minLength={12}
                required
              />
              <small>至少 12 个字符。</small>
            </label>
            <button className="button primary auth-submit" type="submit" disabled={busy}>
              {busy ? "提交中…" : "设置新密码"}
            </button>
            <div className="auth-alt">
              <button
                type="button"
                className="link-button"
                disabled={busy || cooldown > 0}
                onClick={() => void requestReset()}
              >
                {cooldown > 0 ? `重新发送（${cooldown}s）` : "重新发送验证码"}
              </button>
            </div>
            <div className="auth-alt">
              <button
                type="button"
                className="link-button"
                disabled={busy}
                onClick={() => goto("signIn")}
              >
                返回登录
              </button>
            </div>
          </form>
        ) : null}

        {step === "signUpForm" ? (
          <form
            className="auth-form"
            onSubmit={(event) => {
              event.preventDefault();
              void signUp();
            }}
          >
            <label className="field code">
              <span>邀请码</span>
              <input
                ref={firstField}
                value={inviteCode}
                placeholder="DM-XXXX-XXXX-XXXX"
                disabled={busy}
                onChange={(event) => setInviteCode(event.target.value)}
                required
              />
            </label>
            <label className="field">
              <span>邮箱</span>
              <input
                type="email"
                autoComplete="username"
                placeholder="you@example.com"
                value={email}
                disabled={busy}
                onChange={(event) => setEmail(event.target.value)}
                required
              />
            </label>
            <label className="field">
              <span>密码</span>
              <input
                type="password"
                autoComplete="new-password"
                minLength={12}
                value={password}
                disabled={busy}
                onChange={(event) => setPassword(event.target.value)}
                required
              />
              <small>
                至少 12 位。
                {password.length > 0 && password.length < 12
                  ? `还差 ${12 - password.length} 位。`
                  : ""}
              </small>
            </label>
            <label className="field">
              <span>昵称</span>
              <input
                value={alias}
                maxLength={80}
                placeholder="标注列表里显示的名字"
                disabled={busy}
                onChange={(event) => setAlias(event.target.value)}
                required
              />
            </label>

            {terms ? (
              <>
                <div className="auth-terms">
                  <header>
                    <span>贡献者条款</span>
                    <small>{terms.policyVersion}</small>
                  </header>
                  <div
                    className="auth-terms-body"
                    tabIndex={0}
                    role="region"
                    aria-label="贡献者条款正文"
                  >
                    {terms.body}
                  </div>
                </div>
                <label className="auth-consent">
                  <input
                    type="checkbox"
                    checked={accepted}
                    disabled={busy}
                    onChange={(event) => setAccepted(event.target.checked)}
                  />
                  <span>我已阅读并同意上述条款</span>
                </label>
              </>
            ) : termsStatus === "failed" ? (
              // 没有条款正文就没有可同意的对象，注册按钮此时是灰的。所以这里必须
              // 给出重试，否则这一步是个死胡同——只能退回上一页重来。
              <div className="auth-terms-unavailable">
                <p className="auth-note">
                  条款没有取到。在读到条款之前不能表示同意，注册也就无法继续。
                </p>
                <button
                  type="button"
                  className="link-button"
                  disabled={busy}
                  onClick={() => void loadTerms()}
                >
                  重新获取条款
                </button>
              </div>
            ) : (
              <p className="auth-note">正在获取当前条款…</p>
            )}

            <button
              className="button primary auth-submit"
              type="submit"
              disabled={busy || !terms || !accepted}
            >
              {busy ? "提交中…" : "注册"}
            </button>
            <div className="auth-alt">
              <button
                type="button"
                className="link-button quiet"
                disabled={busy}
                onClick={abandonSignUp}
              >
                返回
              </button>
              <button
                type="button"
                className="link-button"
                disabled={busy}
                onClick={() => {
                  setInSignUpFlow(false);
                  goto("signIn");
                }}
              >
                已有账号，去登录
              </button>
            </div>
          </form>
        ) : null}

        {step === "signUpVerify" ? (
          <form
            className="auth-form"
            onSubmit={(event) => {
              event.preventDefault();
              void verify();
            }}
          >
            <label className="field digits">
              <span>验证码</span>
              <input
                ref={firstField}
                inputMode="numeric"
                autoComplete="one-time-code"
                maxLength={6}
                value={verificationCode}
                disabled={busy}
                // 只留数字：从邮件里复制常常会带上空格或换行，让它静静掉掉比
                // 弹一个「格式不对」有用。
                onChange={(event) =>
                  setVerificationCode(event.target.value.replace(/\D/g, "").slice(0, 6))
                }
                required
              />
            </label>
            <button
              className="button primary auth-submit"
              type="submit"
              disabled={busy || verificationCode.length !== 6}
            >
              {busy ? "验证中…" : "验证"}
            </button>
            <div className="auth-alt">
              <button
                type="button"
                className="link-button quiet"
                disabled={busy}
                onClick={abandonSignUp}
              >
                返回
              </button>
              <button
                type="button"
                className="link-button"
                disabled={busy || cooldown > 0}
                onClick={() => void resend()}
              >
                {cooldown > 0 ? `重发验证码（${cooldown}s）` : "重发验证码"}
              </button>
            </div>
          </form>
        ) : null}
      </div>
    </section>
  );
}

export function AccountStatusReadPlaceholder({ failed }: { failed: boolean }) {
  const copy = accountStatusReadCopy(failed);
  return (
    <section className="account-view">
      <div className="auth">
        <header className="auth-head">
          <LogoMark />
          <span className="section-kicker">ANNOTATOR ACCOUNT</span>
          <h2>{copy.title}</h2>
          <p>{copy.body}</p>
        </header>
      </div>
    </section>
  );
}

function SignUpProgress({ current }: { current: Step }) {
  const at = SIGNUP_STEPS.findIndex((s) => s.key === current);
  return (
    <div className="auth-steps">
      {SIGNUP_STEPS.map((s, index) => (
        <div
          key={s.key}
          className={`auth-step${index === at ? " active" : ""}${index < at ? " done" : ""}`}
        >
          <span>{index < at ? "✓" : `0${index + 1}`}</span>
          <strong>{s.label}</strong>
        </div>
      ))}
    </div>
  );
}

/**
 * 条款与授权。
 *
 * 存在的理由：consent 此前只在注册那一刻写入，此后没有任何自助路径——换了条款
 * 版本，或同一份文书对「是否授权训练用途」的部署解读变了，本人都无从得知，更
 * 无从补签。这里把两件事变成可见的：现在签的是哪一版，以及它是否已经落后。
 *
 * 落后时才展示签署入口；正文读不到就不给勾选框——对读不到的文书表示同意，
 * 不算同意（与注册表单同一条纪律）。
 */
function ConsentPanel({
  consentState,
  terms,
  busy,
  accepted,
  onAcceptedChange,
  onRenew,
}: {
  consentState: ConsentState | null;
  terms: Terms | null;
  busy: boolean;
  accepted: boolean;
  onAcceptedChange: (accepted: boolean) => void;
  onRenew: () => void;
}) {
  if (!consentState) return null;
  const { consent, matchesCurrentTerms, currentPolicyVersion, currentAllowsTrainableUse } =
    consentState;
  // 老服务端不返回 pendingScopes；缺省空数组让这一段整体不显示，而不是报错。
  const pendingScopes = consentState.pendingScopes ?? [];

  return (
    <div className="auth-card">
      <div className="auth-card-head">
        <ShieldIcon />
        <strong>条款与授权</strong>
      </div>
      <dl>
        <div className="row">
          <dt>已签署版本</dt>
          <dd>{consent ? consent.policyVersion : "尚未签署"}</dd>
        </div>
        {consent ? (
          <div className="row">
            <dt>训练用途</dt>
            <dd>{consent.allowsTrainableUse ? "已授权" : "未授权"}</dd>
          </div>
        ) : null}
      </dl>

      {matchesCurrentTerms ? (
        <p className="muted">这是当前生效的版本，无需操作。</p>
      ) : (
        <>
          {pendingScopes.length > 0 ? (
            // 差集放在最前，且比版本串醒目——「补签能多授权什么」是采集员真正
            // 需要用来做决定的信息，两个版本号的对比不是（docs/108 §1.3 缺陷 B）。
            <p className="consent-pending-scopes">
              <strong>补签后新增授权：</strong>
              {pendingScopes.map(consentScopeLabel).join("、")}
            </p>
          ) : null}
          <p className="muted">
            当前生效的是 {currentPolicyVersion}
            {currentAllowsTrainableUse ? "（授权训练用途）" : "（不授权训练用途）"}。
            签署后，此后采集的数据按新版本归属；已采集的数据仍按当时签署的版本，不受影响
            ——换版不是撤回。
          </p>
          {terms ? (
            <>
              <textarea
                className="terms-body"
                readOnly
                value={terms.body}
                aria-label="贡献者条款正文"
              />
              <label className="auth-consent">
                <input
                  type="checkbox"
                  checked={accepted}
                  disabled={busy}
                  onChange={(event) => onAcceptedChange(event.target.checked)}
                />
                <span>我已阅读并同意上述条款</span>
              </label>
              <button
                type="button"
                className="button primary"
                disabled={busy || !accepted}
                onClick={onRenew}
              >
                {busy ? "签署中…" : "签署当前条款"}
              </button>
            </>
          ) : (
            // 正文没取到就不给可勾选的对象——这与注册表单是同一条纪律。
            <p className="muted">条款正文没有取到，在读到之前无法签署。</p>
          )}
        </>
      )}
    </div>
  );
}

function AccountOverview({
  status,
  environment,
  busy,
  deviceBusy,
  deviceStatus,
  onEnrollDevice,
  onReloadDevices,
  onSignIn,
  onSignUp,
  onSignOut,
  consentState,
  terms,
  consentAccepted,
  onConsentAcceptedChange,
  onRenewConsent,
}: {
  status: AccountStatus;
  environment: AccountEnvironment;
  busy: boolean;
  deviceBusy: boolean;
  deviceStatus: DeviceEnrollmentStatus | null;
  onEnrollDevice: () => void;
  onReloadDevices: () => void;
  onSignIn: () => void;
  onSignUp: () => void;
  onSignOut: () => void;
  consentState: ConsentState | null;
  terms: Terms | null;
  consentAccepted: boolean;
  onConsentAcceptedChange: (accepted: boolean) => void;
  onRenewConsent: () => void;
}) {
  if (status.identity.kind === "signedIn") {
    return (
      <>
        <div className="auth-identity">
          <div>
            <ShieldIcon />
            <strong>已登录</strong>
          </div>
          <dl>
            <div className="row">
              <dt>标注员 ID</dt>
              <dd>{status.identity.annotatorId.slice(0, 8)}</dd>
            </div>
          </dl>
        </div>

        <DevicePanel
          environment={environment}
          deviceBusy={deviceBusy}
          deviceStatus={deviceStatus}
          onEnrollDevice={onEnrollDevice}
          onReloadDevices={onReloadDevices}
        />

        <ConsentPanel
          consentState={consentState}
          terms={terms}
          busy={busy}
          accepted={consentAccepted}
          onAcceptedChange={onConsentAcceptedChange}
          onRenew={onRenewConsent}
        />

        <SelfServicePanel />

        <div className="auth-alt">
          <span>不是你的机器？</span>
          <button
            type="button"
            className="link-button"
            disabled={busy || deviceBusy}
            onClick={onSignOut}
          >
            {busy ? "退出中…" : "退出登录"}
          </button>
        </div>
      </>
    );
  }

  if (status.identity.kind === "unknown") {
    return (
      <>
        <p className="auth-note">
          账号状态未知——本机存有登录凭据，但暂时连不上账号服务。已有本地采集文件不受影响；新采集可用性请以采集页为准。
        </p>
        <button className="button primary auth-submit" type="button" onClick={onSignIn}>
          重新登录
        </button>
      </>
    );
  }

  return (
    <>
      <button className="button primary auth-submit" type="button" onClick={onSignIn}>
        登录
      </button>
      <div className="auth-alt">
        <span>持有邀请码？</span>
        <button type="button" className="link-button" onClick={onSignUp}>
          注册新账号
        </button>
      </div>
    </>
  );
}

/**
 * 设备登记面（docs/11 §7 的 enroll 闭环）。
 *
 * 「已登记」是两个条件的合取：服务端登记为 active/pending，**且**本地存有设备凭据。
 * 只满足前者是一个真实且会咬人的状态——幂等 enroll 不会补发凭据。PRODUCTION 要说明
 * 上传资格缺失；STAGING 则只说明登记/凭据事实并重申上传不在本次验收范围，不能假装
 * 重复 enroll 会补发凭据。
 */
function DevicePanel({
  environment,
  deviceBusy,
  deviceStatus,
  onEnrollDevice,
  onReloadDevices,
}: {
  environment: AccountEnvironment;
  deviceBusy: boolean;
  deviceStatus: DeviceEnrollmentStatus | null;
  onEnrollDevice: () => void;
  onReloadDevices: () => void;
}) {
  if (!deviceStatus) {
    return (
      <div className="device-panel">
        <div className="device-head">
          <span className="section-kicker cyan">THIS MACHINE</span>
          {!deviceBusy ? (
            <button type="button" className="link-button" onClick={onReloadDevices}>
              重试
            </button>
          ) : null}
        </div>
        <p className="auth-note">
          {deviceBusy ? "正在读取设备登记状态…" : "设备登记状态尚未载入。"}
        </p>
      </div>
    );
  }

  const current = deviceStatus.devices.find(
    (device) => device.clientDeviceId === deviceStatus.clientDeviceId,
  );
  const registered = current?.status === "active" || current?.status === "pending";
  const enrolled = Boolean(current && registered && deviceStatus.deviceCredentialPresent);

  const state = enrolled
    ? { tone: "ok", copy: deviceEnrollmentStateCopy(environment, { kind: "enrolled" }) }
    : current && !registered
      ? {
          tone: "warn",
          copy: deviceEnrollmentStateCopy(environment, {
            kind: "inactive",
            status: current.status,
          }),
        }
      : current
        ? {
            tone: "warn",
            copy: deviceEnrollmentStateCopy(environment, { kind: "credentialMissing" }),
          }
        : {
            tone: "idle",
            copy: deviceEnrollmentStateCopy(environment, { kind: "unregistered" }),
          };

  return (
    <div className={`device-panel device-${state.tone}`}>
      <div className="device-head">
        <span className="section-kicker cyan">THIS MACHINE</span>
        <span className={`device-badge device-${state.tone}`}>{enrolled ? "已登记" : "未登记"}</span>
      </div>
      <p className="auth-note">{state.copy}</p>

      <dl className="device-facts">
        <div className="row">
          <dt>安装标识</dt>
          <dd>{deviceStatus.clientDeviceId.slice(0, 12)}…</dd>
        </div>
        {current ? (
          <div className="row">
            <dt>客户端版本</dt>
            <dd>{current.clientVersion}</dd>
          </div>
        ) : null}
      </dl>

      {!current ? (
        <div className="device-enroll">
          <button
            className="button primary auth-submit"
            type="button"
            disabled={deviceBusy}
            onClick={onEnrollDevice}
          >
            {deviceBusy ? "采集设备快照并登记中…" : "登记本机"}
          </button>
        </div>
      ) : null}

      <div className="device-list-head">
        <span className="section-kicker cyan">ACCOUNT DEVICES</span>
        <small>{deviceStatus.devices.length}</small>
      </div>
      {deviceStatus.devices.length === 0 ? (
        <p className="auth-note">这个账号还没有登记设备。</p>
      ) : (
        <ul className="device-list">
          {deviceStatus.devices.map((device) => (
            <li key={device.deviceId}>
              <code>{device.clientDeviceId.slice(0, 12)}…</code>
              <span className={`device-badge device-${device.status === "active" ? "ok" : "warn"}`}>
                {device.status}
              </span>
              <small>客户端 {device.clientVersion}</small>
              {device.clientDeviceId === deviceStatus.clientDeviceId ? <strong>本机</strong> : null}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/**
 * 账号自助面（docs/34 P6）：改密 + 联络资料。折叠为两个小节，默认收起——
 * 它们是低频动作，不该和「登记本机」抢注意力。
 *
 * 改密走 `account_change_password`：服务端验旧密后撤销**其他**会话、保留当前
 * 这份，所以成功后本界面保持登录，不需要重新进。资料是整体替换语义：留空的
 * 字段提交后即被清空。
 */
function SelfServicePanel() {
  const [open, setOpen] = useState<"none" | "password" | "profile">("none");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const [currentPassword, setCurrentPassword] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [confirmPassword, setConfirmPassword] = useState("");

  const [profileLoaded, setProfileLoaded] = useState(false);
  const [displayName, setDisplayName] = useState("");
  const [phone, setPhone] = useState("");
  const [imKind, setImKind] = useState("");
  const [imHandle, setImHandle] = useState("");
  const [region, setRegion] = useState("");

  async function openProfile() {
    setOpen("profile");
    setError(null);
    setNotice(null);
    if (profileLoaded) return;
    try {
      const profile = await invoke<AccountProfile | null>("account_get_profile");
      if (profile) {
        setDisplayName(profile.displayName ?? "");
        setPhone(profile.phone ?? "");
        setImKind(profile.imKind ?? "");
        setImHandle(profile.imHandle ?? "");
        setRegion(profile.region ?? "");
      }
      setProfileLoaded(true);
    } catch (thrown) {
      setError(errorMessage(thrown, "无法读取联络资料，请稍后重试。"));
    }
  }

  async function submitPassword(event: React.FormEvent) {
    event.preventDefault();
    if (newPassword !== confirmPassword) {
      setError("两次输入的新密码不一致。");
      return;
    }
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await invoke("account_change_password", {
        request: { currentPassword, newPassword },
      });
      setCurrentPassword("");
      setNewPassword("");
      setConfirmPassword("");
      setOpen("none");
      setNotice("密码已修改。其他设备上的登录已退出；本机保持登录，设备凭据不受影响。");
    } catch (thrown) {
      setError(errorMessage(thrown, "修改密码失败，请稍后重试。"));
    } finally {
      setBusy(false);
    }
  }

  async function submitProfile(event: React.FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    setNotice(null);
    const request: ProfileUpdate = {
      ...(displayName.trim() ? { displayName: displayName.trim() } : {}),
      ...(phone.trim() ? { phone: phone.trim() } : {}),
      ...(imKind.trim() && imHandle.trim()
        ? { imKind: imKind.trim(), imHandle: imHandle.trim() }
        : {}),
      ...(region.trim() ? { region: region.trim() } : {}),
    };
    try {
      const profile = await invoke<AccountProfile | null>("account_update_profile", { request });
      setDisplayName(profile?.displayName ?? "");
      setPhone(profile?.phone ?? "");
      setImKind(profile?.imKind ?? "");
      setImHandle(profile?.imHandle ?? "");
      setRegion(profile?.region ?? "");
      setOpen("none");
      setNotice("联络资料已保存。");
    } catch (thrown) {
      setError(errorMessage(thrown, "保存联络资料失败，请稍后重试。"));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="device-panel">
      <div className="device-head">
        <span className="section-kicker cyan">ACCOUNT SETTINGS</span>
      </div>

      {notice ? <p className="auth-note">{notice}</p> : null}
      {error ? (
        <div className="auth-banner error" role="alert">
          <div>
            <p>{error}</p>
          </div>
        </div>
      ) : null}

      {open === "none" ? (
        <div className="auth-alt">
          <button type="button" className="link-button" disabled={busy} onClick={() => {
            setOpen("password");
            setError(null);
            setNotice(null);
          }}>
            修改密码
          </button>
          <button type="button" className="link-button" disabled={busy} onClick={() => void openProfile()}>
            联络资料
          </button>
        </div>
      ) : null}

      {open === "password" ? (
        <form className="auth-form" onSubmit={(event) => void submitPassword(event)}>
          <label className="field">
            <span>当前密码</span>
            <input
              type="password"
              autoComplete="current-password"
              value={currentPassword}
              disabled={busy}
              onChange={(event) => setCurrentPassword(event.target.value)}
              required
              autoFocus
            />
          </label>
          <label className="field">
            <span>新密码（至少 12 位，不含邮箱地址）</span>
            <input
              type="password"
              autoComplete="new-password"
              minLength={12}
              value={newPassword}
              disabled={busy}
              onChange={(event) => setNewPassword(event.target.value)}
              required
            />
          </label>
          <label className="field">
            <span>再次输入新密码</span>
            <input
              type="password"
              autoComplete="new-password"
              value={confirmPassword}
              disabled={busy}
              onChange={(event) => setConfirmPassword(event.target.value)}
              required
            />
          </label>
          <button className="button primary auth-submit" type="submit" disabled={busy}>
            {busy ? "提交中…" : "确认修改"}
          </button>
          <div className="auth-alt">
            <button type="button" className="link-button" disabled={busy} onClick={() => setOpen("none")}>
              取消
            </button>
          </div>
        </form>
      ) : null}

      {open === "profile" ? (
        <form className="auth-form" onSubmit={(event) => void submitProfile(event)}>
          <p className="auth-note">
            全部可选，仅用于运营协调采集。留空的字段保存后会被清空。
          </p>
          <label className="field">
            <span>称呼</span>
            <input value={displayName} maxLength={80} disabled={busy}
              onChange={(event) => setDisplayName(event.target.value)} />
          </label>
          <label className="field">
            <span>电话</span>
            <input value={phone} maxLength={32} disabled={busy}
              onChange={(event) => setPhone(event.target.value)} />
          </label>
          <label className="field">
            <span>IM 渠道</span>
            <select value={imKind} disabled={busy} onChange={(event) => setImKind(event.target.value)}>
              <option value="">不填</option>
              <option value="wechat">微信</option>
              <option value="qq">QQ</option>
              <option value="telegram">Telegram</option>
              <option value="discord">Discord</option>
              <option value="other">其他</option>
            </select>
          </label>
          <label className="field">
            <span>IM 帐号{imKind ? "" : "（先选择渠道）"}</span>
            <input value={imHandle} maxLength={128} disabled={busy || !imKind}
              onChange={(event) => setImHandle(event.target.value)} />
          </label>
          <label className="field">
            <span>所在地区</span>
            <input value={region} maxLength={64} disabled={busy}
              onChange={(event) => setRegion(event.target.value)} />
          </label>
          <button className="button primary auth-submit" type="submit" disabled={busy}>
            {busy ? "保存中…" : "保存资料"}
          </button>
          <div className="auth-alt">
            <button type="button" className="link-button" disabled={busy} onClick={() => setOpen("none")}>
              取消
            </button>
          </div>
        </form>
      ) : null}
    </div>
  );
}

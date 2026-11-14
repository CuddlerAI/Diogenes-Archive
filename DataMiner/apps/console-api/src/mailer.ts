import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { randomUUID } from "node:crypto";

import type { ConsoleConfig } from "./config.js";

export type ConsoleMail = {
  to: string;
  subject: string;
  body: string;
  /** Optional branded HTML. `body` stays the plain-text fallback and the
   * machine-readable form (the code / link live there too). */
  html?: string;
};

export interface Mailer {
  send(mail: ConsoleMail): Promise<void>;
}

export class RecipientNotAllowedError extends Error {
  constructor() {
    super("recipient is outside the console mail allowlist");
    this.name = "RecipientNotAllowedError";
  }
}

function isAllowed(recipient: string, allowlist: string[]): boolean {
  if (allowlist.length === 0) return true;
  const address = recipient.toLowerCase();
  const domain = address.slice(address.indexOf("@") + 1);
  return allowlist.some((entry) => entry === address || entry === domain || entry === `@${domain}`);
}

export function buildMailer(config: ConsoleConfig): Mailer {
  const delivery = config.mail.mode === "capture" ? new CaptureMailer(config) : new ResendMailer(config);
  return new AllowlistMailer(delivery, config.mail.allowlist);
}

class AllowlistMailer implements Mailer {
  readonly #delivery: Mailer;
  readonly #allowlist: string[];

  constructor(delivery: Mailer, allowlist: string[]) {
    this.#delivery = delivery;
    this.#allowlist = allowlist;
  }

  async send(mail: ConsoleMail): Promise<void> {
    if (!isAllowed(mail.to, this.#allowlist)) {
      throw new RecipientNotAllowedError();
    }
    await this.#delivery.send(mail);
  }
}

// Development only. Codes land in a local outbox file so the flow is testable
// without a provider; they are never written to the application log.
class CaptureMailer implements Mailer {
  readonly #directory: string;

  constructor(config: ConsoleConfig) {
    this.#directory = config.mail.captureDir;
  }

  async send(mail: ConsoleMail): Promise<void> {
    await mkdir(this.#directory, { recursive: true });
    const file = path.join(this.#directory, `${randomUUID()}.json`);
    await writeFile(file, `${JSON.stringify(mail, null, 2)}\n`, { encoding: "utf8", mode: 0o600 });
  }
}

class ResendMailer implements Mailer {
  readonly #apiKey: string;
  readonly #from: string;

  constructor(config: ConsoleConfig) {
    if (!config.mail.apiKey) throw new Error("resend delivery requires an API key");
    this.#apiKey = config.mail.apiKey;
    this.#from = config.mail.from;
  }

  async send(mail: ConsoleMail): Promise<void> {
    const response = await fetch("https://api.resend.com/emails", {
      method: "POST",
      headers: {
        authorization: `Bearer ${this.#apiKey}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        from: this.#from,
        to: [mail.to],
        subject: mail.subject,
        text: mail.body,
        ...(mail.html ? { html: mail.html } : {}),
      }),
      signal: AbortSignal.timeout(10_000),
    });
    if (!response.ok) {
      throw new Error(`mail delivery failed with status ${response.status}`);
    }
  }
}

// Branded HTML for the transactional emails. Same shape as the Loopback
// templates — warm neutral canvas, white rounded card, muted copy, one accent —
// re-skinned for the DataMiner console. Inline styles only: Gmail and most
// clients drop <style> blocks but keep inline. The plain-text `body` is preserved
// as the fallback and as the machine-readable form.
const ACCENT = "#1f6feb";

function emailShell(inner: string, brand = "DataMiner Console"): string {
  return (
    `<div style="font-family:'Inter',-apple-system,'Segoe UI',Roboto,'PingFang SC',sans-serif;background:#f1f0ee;padding:32px 16px">` +
    `<div style="max-width:480px;margin:0 auto;background:#ffffff;border:1px solid #e4e0dc;border-radius:16px;padding:32px">` +
    `<div style="font-size:20px;font-weight:600;color:#1f2328;letter-spacing:-.01em">${brand} ` +
    `<span style="font-size:12px;font-weight:600;color:#9a6a00;background:#fff4d6;border:1px solid #f0dca0;border-radius:999px;padding:2px 8px;vertical-align:2px">staging</span></div>` +
    inner +
    `<div style="color:#9b9791;font-size:12px;line-height:1.5;margin-top:24px;border-top:1px solid #e4e0dc;padding-top:16px">DataMiner 运营控制台 · 系统自动发送，请勿回复。</div>` +
    `</div></div>`
  );
}

function ctaButton(url: string, label: string): string {
  return (
    `<a href="${url}" style="display:inline-block;background:${ACCENT};color:#ffffff;font-weight:600;` +
    `font-size:14px;padding:11px 22px;border-radius:12px;text-decoration:none">${label}</a>`
  );
}

export function otpMail(code: string, ttlMinutes: number): Omit<ConsoleMail, "to"> {
  const html = emailShell(
    `<div style="color:#716f6c;font-size:14px;line-height:1.6;margin:18px 0 12px">你的登录验证码</div>` +
      `<div style="font-size:32px;font-weight:700;letter-spacing:10px;color:#1f2328;background:#f6f5f3;` +
      `border:1px solid #e4e0dc;border-radius:12px;padding:18px 0;text-align:center;` +
      `font-family:'SFMono-Regular',Menlo,Consolas,monospace">${code}</div>` +
      `<div style="color:#716f6c;font-size:13px;line-height:1.6;margin-top:16px">` +
      `${ttlMinutes} 分钟内有效，仅可使用一次。这是一次登录尝试触发的；若不是你本人，忽略即可——没有它，登录不会成功。</div>`,
  );
  return {
    subject: "DataMiner console verification code",
    body: [
      `Your verification code is ${code}.`,
      `It expires in ${ttlMinutes} minutes and can be used once.`,
      "",
      "This code was requested by a sign-in attempt to the DataMiner console.",
      "If that was not you, the attempt failed without it: do not share this code.",
    ].join("\n"),
    html,
  };
}

export function inviteMail(url: string, ttlHours: number): Omit<ConsoleMail, "to"> {
  const html = emailShell(
    `<div style="color:#716f6c;font-size:14px;line-height:1.6;margin:18px 0 22px">` +
      `你被邀请加入 DataMiner 运营控制台。点击下方按钮设置密码，即可激活账号。</div>` +
      ctaButton(url, "设置密码") +
      `<div style="color:#9b9791;font-size:12px;line-height:1.6;margin-top:16px">链接 ${ttlHours} 小时内有效，仅可使用一次。` +
      `若按钮打不开，复制这个地址：<br><span style="color:#716f6c;word-break:break-all">${url}</span></div>`,
  );
  return {
    subject: "DataMiner console invitation",
    body: [
      "You have been invited to the DataMiner operations console.",
      `Set your password here: ${url}`,
      `The link expires in ${ttlHours} hours and can be used once.`,
    ].join("\n"),
    html,
  };
}

// The annotator (end-user) signup/verification code. Same card, "DataMiner"
// brand rather than "DataMiner Console" — annotators are not operators.
export function annotatorOtpMail(code: string, ttlMinutes: number): Omit<ConsoleMail, "to"> {
  const html = emailShell(
    `<div style="color:#716f6c;font-size:14px;line-height:1.6;margin:18px 0 12px">验证你的 DataMiner 账号</div>` +
      `<div style="font-size:32px;font-weight:700;letter-spacing:10px;color:#1f2328;background:#f6f5f3;` +
      `border:1px solid #e4e0dc;border-radius:12px;padding:18px 0;text-align:center;` +
      `font-family:'SFMono-Regular',Menlo,Consolas,monospace">${code}</div>` +
      `<div style="color:#716f6c;font-size:13px;line-height:1.6;margin-top:16px">` +
      `${ttlMinutes} 分钟内有效，仅可使用一次。若不是你本人操作，忽略即可。</div>`,
    "DataMiner",
  );
  return {
    subject: "验证你的 DataMiner 账号",
    body: [
      `验证码：${code}`,
      `${ttlMinutes} 分钟内有效，只能使用一次。`,
      "如果不是你本人操作，请忽略本邮件。",
    ].join("\n"),
    html,
  };
}

/**
 * Sent when someone tries to sign up with an address that already has an
 * account (11 §7.1). Signup answers the same way whether the address was free
 * or taken, and this mail is the other half of that: without it the two cases
 * differ by whether a message arrives, and the wall-clock cost of sending is
 * itself a signal. It also has standalone value — the actual owner learns that
 * someone tried to register as them.
 *
 * It deliberately carries no code and no link. There is nothing to act on.
 */
export function annotatorAddressInUseMail(): Omit<ConsoleMail, "to"> {
  const html = emailShell(
    `<div style="color:#716f6c;font-size:14px;line-height:1.6;margin:18px 0 12px">` +
      `刚刚有人用这个邮箱注册 DataMiner，但它已经有账号了。</div>` +
      `<div style="color:#716f6c;font-size:13px;line-height:1.6;margin-top:16px">` +
      `如果是你本人，直接用原来的密码登录即可；忘记密码可以在客户端重置。<br>` +
      `如果不是你，也无需处理——没有你的密码，注册不会成功，这封信只是让你知道。</div>`,
    "DataMiner",
  );
  return {
    subject: "这个邮箱已注册 DataMiner",
    body: [
      "刚刚有人用这个邮箱注册 DataMiner，但它已经有账号了。",
      "如果是你本人，直接用原来的密码登录即可。",
      "如果不是你，无需处理：没有你的密码，注册不会成功。",
    ].join("\n"),
    html,
  };
}

/**
 * The claim link for an operator-registered account (docs/34 §5.2): the
 * annotator opens it, reads the terms, and sets their own password. Carries a
 * link rather than a code because the recipient has no account to sign in to
 * yet — the link is the whole ceremony.
 */
export function annotatorClaimMail(url: string, ttlHours: number): Omit<ConsoleMail, "to"> {
  const html = emailShell(
    `<div style="color:#716f6c;font-size:14px;line-height:1.6;margin:18px 0 22px">` +
      `你的 DataMiner 采集账号已由运营创建。点击下方按钮阅读条款并设置密码，账号即激活。</div>` +
      ctaButton(url, "设置密码并激活") +
      `<div style="color:#9b9791;font-size:12px;line-height:1.6;margin-top:16px">链接 ${ttlHours} 小时内有效，仅可使用一次。` +
      `若按钮打不开，复制这个地址：<br><span style="color:#716f6c;word-break:break-all">${url}</span></div>`,
    "DataMiner",
  );
  return {
    subject: "你的 DataMiner 采集账号已创建",
    body: [
      "你的 DataMiner 采集账号已由运营创建。",
      `阅读条款并设置密码：${url}`,
      `链接 ${ttlHours} 小时内有效，仅可使用一次。`,
    ].join("\n"),
    html,
  };
}

/**
 * The operator-triggered reset link (docs/34 §6.5). Distinct copy from the
 * self-service six digit code: this one was initiated *for* the user, so the
 * mail says so, and says what to do if that is a surprise.
 */
export function annotatorResetLinkMail(url: string, ttlMinutes: number): Omit<ConsoleMail, "to"> {
  const html = emailShell(
    `<div style="color:#716f6c;font-size:14px;line-height:1.6;margin:18px 0 22px">` +
      `运营为你的 DataMiner 账号发起了密码重置。点击下方按钮设置新密码；完成后所有旧登录都会失效。</div>` +
      ctaButton(url, "设置新密码") +
      `<div style="color:#9b9791;font-size:12px;line-height:1.6;margin-top:16px">链接 ${ttlMinutes} 分钟内有效，仅可使用一次。` +
      `若这不是你请求的，请联系运营；不点击链接则什么都不会发生。<br>` +
      `若按钮打不开，复制这个地址：<br><span style="color:#716f6c;word-break:break-all">${url}</span></div>`,
    "DataMiner",
  );
  return {
    subject: "DataMiner 账号密码重置",
    body: [
      "运营为你的 DataMiner 账号发起了密码重置。",
      `设置新密码：${url}`,
      `链接 ${ttlMinutes} 分钟内有效，仅可使用一次。完成后所有旧登录都会失效。`,
      "若这不是你请求的，请联系运营；不点击链接则什么都不会发生。",
    ].join("\n"),
    html,
  };
}

export function passwordResetMail(url: string, ttlMinutes: number): Omit<ConsoleMail, "to"> {
  const html = emailShell(
    `<div style="color:#716f6c;font-size:14px;line-height:1.6;margin:18px 0 22px">` +
      `收到你的 DataMiner 控制台密码重置请求。点击下方按钮设置新密码。</div>` +
      ctaButton(url, "重置密码") +
      `<div style="color:#9b9791;font-size:12px;line-height:1.6;margin-top:16px">链接 ${ttlMinutes} 分钟内有效，仅可使用一次。` +
      `若不是你发起的，忽略本邮件即可，无需任何操作。</div>`,
  );
  return {
    subject: "DataMiner console password reset",
    body: [
      "A password reset was requested for your DataMiner console account.",
      `Set a new password here: ${url}`,
      `The link expires in ${ttlMinutes} minutes and can be used once.`,
      "",
      "If you did not request this, no action is required.",
    ].join("\n"),
    html,
  };
}

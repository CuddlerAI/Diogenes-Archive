import { parseArgs } from "node:util";

import { loadConfig } from "./config.js";
import { emailSchema } from "./contracts.js";
import { PostgresConsoleRepository } from "./repository.js";
import { createPool } from "./sql.js";
import { generateOpaqueToken, generateSalt, hashChallengeCode } from "./secrets.js";

// The first administrator is created before any operator can sign in, so the
// invitation link is printed to the operator terminal instead of being mailed:
// at bootstrap time the mail channel has not been verified yet.
const { values } = parseArgs({
  options: {
    email: { type: "string" },
    role: { type: "string", default: "admin" },
  },
});

if (!values.email) {
  process.stderr.write("usage: bootstrap-operator --email <address> [--role admin|operator|viewer]\n");
  process.exit(2);
}
const role = values.role ?? "admin";
if (role !== "admin" && role !== "operator" && role !== "viewer") {
  process.stderr.write(`unsupported role: ${role}\n`);
  process.exit(2);
}

const email = emailSchema.parse(values.email);
const config = loadConfig();
const pool = createPool(config.databaseUrl, 2);
const repository = new PostgresConsoleRepository(pool, () => pool.end());

try {
  const existing = await repository.findOperatorByEmail(email);
  // An operator that was bootstrapped but never activated can be handed a fresh
  // link. Refusing outright is how the first administrator gets locked out for
  // good: the invite expires after CONSOLE_INVITE_TTL_HOURS, this command used
  // to exit here, the console's own invite endpoint needs an admin that nobody
  // can sign in as yet, password reset returns silently for a non-active
  // operator, and DELETE is refused by a trigger. Raw SQL was the only way back.
  //
  // Anything past `invited` is refused, as before. Re-issuing for an active
  // operator would be a password reset that never proves control of the mailbox.
  if (existing && existing.status !== "invited") {
    // Report the role too: "already active" answers "do I need an invite?", but
    // "already active as viewer" is a different situation from "already admin".
    process.stderr.write(
      `operator already exists and is ${existing.status} with role ${existing.role}: ${email}\n`,
    );
    process.exit(1);
  }
  if (existing && existing.role !== role) {
    process.stderr.write(
      `pending invite for ${email} has role ${existing.role}, not ${role}; re-run with --role ${existing.role}\n`,
    );
    process.exit(1);
  }
  if (existing) {
    // The previous link stops working the moment this one is issued.
    await repository.supersedeOpenChallenges(existing.id, "invite");
  }

  const operator =
    existing ?? (await repository.createOperator({ email, role, createdBy: null }));
  const token = generateOpaqueToken();
  const salt = generateSalt();
  const challenge = await repository.issueChallenge({
    operatorId: operator.id,
    purpose: "invite",
    codeSalt: salt,
    codeHash: hashChallengeCode(salt, token),
    ttlSeconds: config.challenge.inviteTtlHours * 3_600,
  });
  await repository.recordAudit({
    actorType: "service",
    actorId: "bootstrap-operator",
    action: existing ? "operator.invite_reissued" : "operator.bootstrapped",
    entityType: "operator",
    entityId: operator.id,
    details: { role, reissued: existing !== null },
  });

  const url = new URL("/accept-invite", config.publicOrigin);
  url.searchParams.set("challenge", challenge.id);
  url.searchParams.set("token", token);
  process.stdout.write(
    [
      existing ? `re-issued invite for ${role} operator ${email}` : `created ${role} operator ${email}`,
      `set the password within ${config.challenge.inviteTtlHours}h using:`,
      url.toString(),
      "",
    ].join("\n"),
  );
} finally {
  await repository.close();
}

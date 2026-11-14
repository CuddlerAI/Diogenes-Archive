import { randomInt } from "node:crypto";

/**
 * Invite codes — normalization, generation, display.
 *
 * The shape follows Loopback's invite implementation, which got the important
 * parts right (docs/11 §5.1): the normalized plaintext is the primary key, one
 * `normalize` serves both issuing and redemption so the two cannot drift, and a
 * de-ambiguated alphabet keeps a hand-typed code from turning into a support
 * ticket.
 *
 * Two deliberate departures from Loopback (11 §5.2):
 *
 *   * three groups instead of two — 60 bits rather than 40. Loopback's 40-bit
 *     argument rests on "codes are issued in advance and signup is rate limited
 *     per IP". Here the signup endpoint faces the public internet from a desktop
 *     client, an attacker can spread across an address pool, and guessing right
 *     buys entry to a data collection programme rather than a SaaS trial. Five
 *     more characters to type is a cheap price for 20 more bits.
 *   * `DM` prefix, so a code is recognisable as this system's.
 *
 * Why the code is not hashed at rest is argued in 11 §12.2 and repeated in the
 * migration; the short version is that an invite code is a registration gate,
 * not a credential, and operations has to be able to match a code to the person
 * it was issued to.
 */

// Crockford-ish: no 0/O/1/I/L. A human retypes this from a chat message into a
// desktop app, and those four pairs are where that goes wrong.
const ALPHABET = "23456789ABCDEFGHJKMNPQRSTUVWXYZ";
const PREFIX = "DM";
const GROUPS = 3;
const GROUP_LENGTH = 4;
const BODY_LENGTH = GROUPS * GROUP_LENGTH;

/** The stored and compared form: `DM` followed by 12 alphabet characters. */
export const normalizedPattern = new RegExp(`^${PREFIX}[${ALPHABET}]{${BODY_LENGTH}}$`);

/**
 * Strip separators and case-fold. Pasting with or without dashes, in any case,
 * has to reach the same row — and issuing has to produce that same form, which
 * is why both paths call this one function.
 */
export function normalize(raw: string | null | undefined): string {
  return (raw ?? "").replace(/[^0-9A-Za-z]/g, "").toUpperCase();
}

/** A newly minted code in display form, `DM-XXXX-XXXX-XXXX`. */
export function generate(): string {
  const body = Array.from({ length: GROUPS }, () =>
    Array.from({ length: GROUP_LENGTH }, () => ALPHABET[randomInt(0, ALPHABET.length)]).join(""),
  ).join("-");
  return `${PREFIX}-${body}`;
}

/**
 * Normalized form back to display form. The console list and the moment of
 * issuing both render through this, so a code looks the same everywhere it is
 * read. Anything that is not a well-formed normalized code is returned as-is
 * rather than mangled into a plausible-looking one.
 */
export function pretty(normalized: string): string {
  if (!normalizedPattern.test(normalized)) return normalized;
  const body = normalized.slice(PREFIX.length);
  const groups: string[] = [];
  for (let index = 0; index < body.length; index += GROUP_LENGTH) {
    groups.push(body.slice(index, index + GROUP_LENGTH));
  }
  return `${PREFIX}-${groups.join("-")}`;
}

/**
 * Mint `count` distinct codes. Duplicates within a batch are dropped here;
 * collisions against rows already in the table are caught by the primary key.
 * Probing the database per candidate would not prevent a concurrent insert
 * anyway — the constraint is what actually guarantees uniqueness — so it would
 * only add a round trip per code.
 */
export function generateBatch(count: number): string[] {
  const display: string[] = [];
  const seen = new Set<string>();
  while (display.length < count) {
    const code = generate();
    const normalized = normalize(code);
    if (seen.has(normalized)) continue;
    seen.add(normalized);
    display.push(code);
  }
  return display;
}

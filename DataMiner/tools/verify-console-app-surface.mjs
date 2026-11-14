import { fileURLToPath } from 'node:url';

const sha256Hex = /^[0-9a-f]{64}$/;
const revisionName = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;

function fail(message) {
  throw new Error(message);
}

function parseBaseUrl(raw) {
  let url;
  try {
    url = new URL(raw);
  } catch {
    fail('base URL must be an absolute URL');
  }
  const loopback = url.hostname === '127.0.0.1' || url.hostname === 'localhost' || url.hostname === '::1';
  if (url.protocol !== 'https:' && !(url.protocol === 'http:' && loopback)) {
    fail('base URL must use https');
  }
  if (url.username || url.password || url.search || url.hash || !['', '/'].includes(url.pathname)) {
    fail('base URL must be an origin root without user info, path, query, or fragment');
  }
  url.pathname = '/';
  return url;
}

async function requestJson(base, path, init = {}) {
  const response = await fetch(new URL(path, base), {
    redirect: 'manual',
    signal: AbortSignal.timeout(15_000),
    ...init,
  });
  if (response.status >= 300 && response.status < 400) {
    fail(`${path} must not redirect`);
  }
  const contentType = response.headers.get('content-type') ?? '';
  let body = null;
  if (contentType.toLowerCase().includes('application/json')) {
    try {
      body = await response.json();
    } catch {
      fail(`${path} returned malformed JSON`);
    }
  }
  return { response, body };
}

function object(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value) ? value : null;
}

export async function verifyConsoleAppSurface({ baseUrl, environment }) {
  if (environment !== 'staging' && environment !== 'production') {
    fail('environment must be exactly staging or production');
  }
  const base = parseBaseUrl(baseUrl);

  const ready = await requestJson(base, '/readyz');
  if (ready.response.status !== 200) fail('readyz must return HTTP 200');
  const readyBody = object(ready.body);
  if (!readyBody) fail('readyz must return JSON');
  if (readyBody.status !== 'ready' || readyBody.environment !== environment) {
    fail('readyz environment or status does not match the requested deployment');
  }
  if (object(readyBody.dependencies)?.cloudSql?.status !== 'ready') {
    fail('readyz must report Cloud SQL ready');
  }
  if (object(readyBody.features)?.annotatorApi !== true) {
    fail('readyz must report the annotator API enabled');
  }
  const revision = object(readyBody.deployment)?.revision;
  if (typeof revision !== 'string' || !revisionName.test(revision)) {
    fail('readyz must identify the deployed revision');
  }

  // This GET is the fail-closed gate. No POST is sent unless the exact public
  // terms contract succeeds first.
  const terms = await requestJson(base, '/app/v1/terms', {
    headers: { accept: 'application/json' },
  });
  if (terms.response.status !== 200) fail('terms endpoint must return HTTP 200');
  const termsBody = object(terms.body);
  if (
    !termsBody ||
    typeof termsBody.policyVersion !== 'string' || termsBody.policyVersion.length === 0 ||
    typeof termsBody.purpose !== 'string' || termsBody.purpose.length === 0 ||
    typeof termsBody.termsHash !== 'string' || !sha256Hex.test(termsBody.termsHash) ||
    typeof termsBody.body !== 'string' || termsBody.body.length === 0 ||
    typeof termsBody.allowsTrainableUse !== 'boolean' ||
    !Array.isArray(termsBody.scopes) || !termsBody.scopes.every((scope) => typeof scope === 'string')
  ) {
    fail('terms endpoint returned the wrong JSON contract');
  }

  // Empty JSON is intentionally invalid. Reaching input validation proves the
  // login route is public without creating an account, session, or audit row.
  const login = await requestJson(base, '/app/v1/login', {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'application/json' },
    body: '{}',
  });
  if (login.response.status !== 400 || object(login.body)?.code !== 'invalid_request') {
    fail('login endpoint must reach public input validation');
  }

  return {
    environment,
    origin: base.origin,
    revision,
    policyVersion: termsBody.policyVersion,
    termsHash: termsBody.termsHash,
  };
}

function parseArgs(argv) {
  const values = new Map();
  for (let index = 0; index < argv.length; index += 2) {
    const name = argv[index];
    const value = argv[index + 1];
    if (!name?.startsWith('--') || value === undefined) fail('expected --base-url and --environment');
    values.set(name, value);
  }
  return {
    baseUrl: values.get('--base-url'),
    environment: values.get('--environment'),
  };
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  try {
    const result = await verifyConsoleAppSurface(parseArgs(process.argv.slice(2)));
    process.stdout.write(`${JSON.stringify({ ok: true, ...result })}\n`);
  } catch (error) {
    const message = error instanceof Error ? error.message : 'unknown verification failure';
    process.stderr.write(`console app surface verification failed: ${message}\n`);
    process.exitCode = 1;
  }
}

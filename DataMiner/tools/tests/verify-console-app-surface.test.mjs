import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import test from 'node:test';

import { verifyConsoleAppSurface } from '../verify-console-app-surface.mjs';

const validTerms = {
  policyVersion: '2026-07-24-staging',
  purpose: 'staging capture',
  termsHash: 'a'.repeat(64),
  body: 'staging terms',
  allowsTrainableUse: false,
  scopes: ['input', 'video'],
};

function sendJson(response, status, body) {
  response.writeHead(status, { 'content-type': 'application/json; charset=utf-8' });
  response.end(JSON.stringify(body));
}

async function withServer(handler, run) {
  const server = createServer(handler);
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const address = server.address();
  assert.ok(address && typeof address === 'object');
  try {
    await run(`http://127.0.0.1:${address.port}/`);
  } finally {
    await new Promise((resolve, reject) => server.close((error) => (error ? reject(error) : resolve())));
  }
}

test('accepts only a revision-identifiable public account surface', async () => {
  const requests = [];
  await withServer((request, response) => {
    requests.push(`${request.method} ${request.url}`);
    if (request.url === '/readyz') {
      sendJson(response, 200, {
        status: 'ready',
        environment: 'staging',
        deployment: { revision: 'dataminer-console-api-staging-00042-abc' },
        dependencies: { cloudSql: { status: 'ready' } },
        features: { annotatorApi: true },
      });
      return;
    }
    if (request.url === '/app/v1/terms') {
      sendJson(response, 200, validTerms);
      return;
    }
    if (request.url === '/app/v1/login') {
      sendJson(response, 400, { code: 'invalid_request' });
      return;
    }
    sendJson(response, 404, { code: 'not_found' });
  }, async (baseUrl) => {
    const result = await verifyConsoleAppSurface({ baseUrl, environment: 'staging' });
    assert.equal(result.revision, 'dataminer-console-api-staging-00042-abc');
  });
  assert.deepEqual(requests, [
    'GET /readyz',
    'GET /app/v1/terms',
    'POST /app/v1/login',
  ]);
});

test('rejects a JSON 401 terms response and never advances to the login POST', async () => {
  let loginRequests = 0;
  await withServer((request, response) => {
    if (request.url === '/readyz') {
      sendJson(response, 200, {
        status: 'ready',
        environment: 'staging',
        deployment: { revision: 'dataminer-console-api-staging-00042-abc' },
        dependencies: { cloudSql: { status: 'ready' } },
        features: { annotatorApi: true },
      });
      return;
    }
    if (request.url === '/app/v1/terms') {
      sendJson(response, 401, { code: 'session_required' });
      return;
    }
    if (request.url === '/app/v1/login') loginRequests += 1;
    sendJson(response, 500, { code: 'unexpected_request' });
  }, async (baseUrl) => {
    await assert.rejects(
      verifyConsoleAppSurface({ baseUrl, environment: 'staging' }),
      /terms endpoint must return HTTP 200/,
    );
  });
  assert.equal(loginRequests, 0);
});

test('rejects a serving deployment that cannot identify its Cloud Run revision', async () => {
  await withServer((request, response) => {
    if (request.url === '/readyz') {
      sendJson(response, 200, {
        status: 'ready',
        environment: 'staging',
        dependencies: { cloudSql: { status: 'ready' } },
        features: { annotatorApi: true },
      });
      return;
    }
    sendJson(response, 500, { code: 'unexpected_request' });
  }, async (baseUrl) => {
    await assert.rejects(
      verifyConsoleAppSurface({ baseUrl, environment: 'staging' }),
      /readyz must identify the deployed revision/,
    );
  });
});

test('both deployment scripts run the strict verifier against the naked Cloud Run URL', async () => {
  const { readFile } = await import('node:fs/promises');
  const staging = await readFile(new URL('../deploy-console-staging.sh', import.meta.url), 'utf8');
  const production = await readFile(new URL('../deploy-console-production.sh', import.meta.url), 'utf8');

  for (const script of [staging, production]) {
    assert.match(script, /verify-console-app-surface\.mjs/);
    assert.doesNotMatch(script, /verify-console-app-surface\.mjs[^\n]*(?:\|\|\s*true|warn)/);
  }
  assert.match(staging, /--base-url\s+"\$\{url\}"[\s\\]*\n[^\n]*--environment\s+staging/);
  assert.match(production, /--base-url\s+"\$\{api_url\}"[\s\\]*\n[^\n]*--environment\s+production/);
});

test('staging redeploy preserves live configuration while adding the registered terms metadata', async () => {
  const { readFile } = await import('node:fs/promises');
  const script = await readFile(new URL('../deploy-console-staging.sh', import.meta.url), 'utf8');
  const deployApi = script.slice(script.indexOf('deploy_api() {'), script.indexOf('deploy_web() {'));

  assert.doesNotMatch(deployApi, /--set-env-vars|--set-secrets/);
  assert.match(deployApi, /--update-env-vars=/);
  assert.match(deployApi, /CONSOLE_APP_TERMS_BODY_PATH=\/app\/terms\/annotator-2026-07-24-staging\.txt/);
  assert.match(deployApi, /CONSOLE_APP_TERMS_ALLOWS_TRAINABLE=true/);
  assert.match(deployApi, /CONSOLE_APP_TERMS_SCOPES=input,video,trainable_research/);

  // Incremental has to stay COMPLETE. Dropping a binding fails the same way as
  // overwriting it — a service that never carried DATABASE_URL will not start,
  // and `secrets` creates the Secret Manager entries without binding any of
  // them. Without these assertions the "no --set-*" rule above is satisfied by
  // a deploy that sets nothing at all.
  assert.match(deployApi, /--update-secrets=/);
  for (const secret of ['DATABASE_URL=', 'CONSOLE_IP_HASH_SALT=',
                        'CONSOLE_PROXY_TOKEN=', 'CONSOLE_MAIL_API_KEY=']) {
    assert.ok(deployApi.includes(secret), `deploy_api must bind ${secret}`);
  }
  for (const env of ['CONSOLE_MAIL_MODE=', 'CONSOLE_MAIL_FROM=', 'CONSOLE_MAIL_ALLOWLIST=']) {
    assert.ok(deployApi.includes(env), `deploy_api must set ${env}`);
  }
});

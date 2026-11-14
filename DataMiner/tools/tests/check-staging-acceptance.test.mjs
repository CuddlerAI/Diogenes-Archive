import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import test from 'node:test';

const dataMinerRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const runbookPath = path.join(dataMinerRoot, 'apps', 'client', 'STAGING-ACCEPTANCE.md');
const runbook = readFileSync(runbookPath, 'utf8');
const runbookLines = runbook.split(/\r?\n/);

function assignmentValues(variableName) {
  const pattern = new RegExp(`\\b${variableName}\\s*=\\s*([^\\s\`]+)`, 'g');
  return [...runbook.matchAll(pattern)].map((match) => match[1]);
}

function contextsContaining(token, linesBefore = 2, linesAfter = 4) {
  return runbookLines.flatMap((line, index) => {
    if (!line.includes(token)) return [];
    return [runbookLines
      .slice(Math.max(0, index - linesBefore), index + linesAfter + 1)
      .join('\n')];
  });
}

test('obsolete account variables appear only in explicit production-risk warnings', () => {
  const obsoleteVariables = [
    'DATAMINER_ACCOUNT_BASE_URL',
  ];

  for (const variableName of obsoleteVariables) {
    assert.deepEqual(
      assignmentValues(variableName),
      [],
      `${variableName} must never be presented as an active assignment`,
    );

    const warningContexts = contextsContaining(variableName);
    assert.ok(warningContexts.length > 0, `${variableName} must retain its migration warning`);
    for (const context of warningContexts) {
      assert.match(context, /没有|不存在|无效|设错/);
      assert.match(context, /production|生产/);
    }
  }
});

test('staging setup and E2E explicitly select the account environment', () => {
  const assignments = assignmentValues('DATAMINER_ACCOUNT_ENVIRONMENT');
  assert.ok(assignments.length >= 2, 'setup and E2E acceptance must both select an environment');
  assert.ok(assignments.every((value) => value === 'staging'));

  const configurationContexts = contextsContaining('DATAMINER_ACCOUNT_ENVIRONMENT', 1, 4);
  assert.ok(configurationContexts.length >= 2);
  assert.ok(configurationContexts.every((context) => context.includes('DATAMINER_ACCOUNT_API_BASE_URL')));
});

test('account API examples use safe HTTPS base roots in setup and E2E acceptance', () => {
  const assignments = assignmentValues('DATAMINER_ACCOUNT_API_BASE_URL');
  assert.ok(assignments.length >= 2, 'setup and E2E acceptance must both configure the account API');
  for (const value of assignments) {
    assert.match(value, /^https:\/\//);
    assert.doesNotMatch(value, /\/app\/v1/i);
    assert.doesNotMatch(value, /[?#]/);
  }

  assert.match(runbook, /必须\s*https/i);
  assert.match(runbook, /不能带[\s\S]{0,80}`?\/app\/v1[\s\S]{0,120}query\/fragment/i);
});

test('staging acceptance keeps credential isolation and pre-production proof fail closed', () => {
  assert.match(runbook, /DiogenesLab\/DataMiner\/staging\/device-credential-v1/);
  assert.match(runbook, /DiogenesLab\/DataMiner\/production\/device-credential-v1/);
  assert.match(runbook, /staging[\s\S]{0,160}(?:独立|隔离)[\s\S]{0,160}Credential Manager/i);

  const proofContexts = contextsContaining('GET /app/v1/terms', 2, 3);
  assert.ok(proofContexts.length > 0, 'runbook must require a staging account API proof request');
  assert.ok(proofContexts.some((context) => /staging[\s\S]*access log/i.test(context)));
  assert.ok(proofContexts.some((context) => /(?:没有|否则)[\s\S]{0,80}(?:停下|生产)/.test(context)));
});

test('terms proof opens the signup form and verifies staging logs before side effects', () => {
  const proofStart = runbook.indexOf('### /terms 只读防误打证明');
  const proofEnd = runbook.indexOf('### 正向（全新安装）', proofStart);
  assert.ok(proofStart >= 0 && proofEnd > proofStart, 'runbook must have a bounded /terms proof procedure');

  const proof = runbook.slice(proofStart, proofEnd);
  const openForm = proof.indexOf('打开邀请码注册表单');
  const readOnlyLog = proof.indexOf('只读核对 staging access log');
  const submitInvite = proof.indexOf('填写并提交邀请码');

  assert.ok(openForm >= 0, 'the procedure must open the form that triggers GET /terms');
  assert.ok(readOnlyLog > openForm, 'staging logs must be checked after opening the form');
  assert.ok(submitInvite > readOnlyLog, 'invite fields must only be submitted after the read-only proof');
  assert.match(proof, /打开邀请码注册表单[\s\S]{0,160}GET `?\/app\/v1\/terms`?/i);
  assert.match(proof, /只读核对 staging access log[\s\S]{0,200}(?:不得|不要)[\s\S]{0,80}(?:POST|提交)/i);
  assert.match(proof, /没有[\s\S]{0,80}GET `?\/app\/v1\/terms`?[\s\S]{0,120}停下/i);
});

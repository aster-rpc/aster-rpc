#!/usr/bin/env node
// Fail unless named vitest files ran with exactly the expected number of
// passing tests and nothing failed or was skipped. A floor in CI: a test file
// that silently stops running (or is skipped wholesale) looks like a pass.
//
//   npx vitest run <files> --reporter=json --outputFile=out.json
//   node ../../scripts/check-vitest-count.mjs out.json observability-logs.test.ts=7 ...
//
// The count in each `name=N` pair must equal the observed count when the
// test file is added or changed.

import { readFileSync } from 'node:fs';

const [, , jsonPath, ...pairs] = process.argv;
if (!jsonPath || pairs.length === 0) {
  console.error('usage: check-vitest-count.mjs <vitest.json> <file-suffix>=<count>...');
  process.exit(2);
}
const report = JSON.parse(readFileSync(jsonPath, 'utf8'));
let bad = false;
for (const pair of pairs) {
  const [suffix, want] = pair.split('=');
  const file = report.testResults.find((r) => r.name.endsWith(suffix));
  if (!file) {
    console.error(`check-vitest-count: ${suffix} did not run`);
    bad = true;
    continue;
  }
  const count = (status) => file.assertionResults.filter((a) => a.status === status).length;
  const passed = count('passed');
  const other = file.assertionResults.length - passed;
  if (passed !== Number(want) || other !== 0) {
    console.error(`check-vitest-count: ${suffix}: expected ${want} passing and none else, got ${passed} passing, ${other} other`);
    bad = true;
  } else {
    console.log(`check-vitest-count: ${suffix}: ${passed} passed`);
  }
}
process.exit(bad ? 1 : 0);

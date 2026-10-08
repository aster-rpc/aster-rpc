import { fileURLToPath } from 'node:url';

/** Explicit diagnostic only; the ordinary TypeScript config excludes this tree. */
export default {
  root: fileURLToPath(new URL('../../../', import.meta.url)),
  test: {
    include: ['tests/assurance/repros/parked-client-stream.test.ts'],
    testTimeout: 30_000,
  },
};

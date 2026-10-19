import { defineConfig } from 'vitest/config';
import { resolve } from 'node:path';
import { TIME_SCALE } from '../../../../tests/typescript/helpers/wait-budget.ts';

export default defineConfig({
  resolve: {
    alias: {
      '@aster-rpc/aster': resolve(__dirname, './src/index.ts'),
    },
  },
  test: {
    include: ['../../../../tests/typescript/**/*.test.ts'],
    // Scaled with the wait budget (tests/typescript/helpers/wait-budget.ts),
    // so a test whose waits stretch under ASTER_TEST_TIME_SCALE is not cut
    // off by the unscaled limit.
    testTimeout: 30_000 * TIME_SCALE,
  },
});

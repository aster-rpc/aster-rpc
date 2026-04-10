import { defineConfig } from 'vitest/config';
import { resolve } from 'node:path';

export default defineConfig({
  resolve: {
    alias: {
      '@aster-rpc/aster': resolve(__dirname, './src/index.ts'),
    },
  },
  test: {
    include: ['../../../../tests/typescript/**/*.test.ts'],
    testTimeout: 30_000,
  },
});

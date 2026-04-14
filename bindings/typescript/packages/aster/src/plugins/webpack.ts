/**
 * Webpack plugin for `aster-gen`.
 *
 * Runs the scanner before each compile (`beforeCompile` hook). Users
 * add it once in `webpack.config.js`:
 *
 * ```js
 * const { AsterGenWebpackPlugin } = require('@aster-rpc/aster/webpack-plugin');
 *
 * module.exports = {
 *   plugins: [
 *     new AsterGenWebpackPlugin({
 *       project: 'tsconfig.json',
 *       out: 'aster-rpc.generated.ts',
 *     }),
 *   ],
 * };
 * ```
 *
 * Like the Vite plugin, this is a thin wrapper around the
 * programmatic {@link generate} entry point in `cli/gen.ts`.
 */

import { generate, ScanError, type GenerateOptions } from '../cli/gen.js';

/**
 * Minimal subset of the Webpack `Compiler` shape we need. We don't
 * import `webpack` directly so users can use this plugin without a
 * hard Webpack dependency.
 */
interface CompilerLike {
  hooks: {
    beforeCompile: {
      tapAsync(
        name: string,
        fn: (_params: unknown, cb: (err?: Error | null) => void) => void,
      ): void;
    };
  };
}

export interface AsterGenWebpackOptions extends GenerateOptions {
  /** Treat scan errors as warnings instead of failing the build. */
  warnOnly?: boolean;
}

export class AsterGenWebpackPlugin {
  constructor(private readonly options: AsterGenWebpackOptions) {}

  apply(compiler: CompilerLike): void {
    compiler.hooks.beforeCompile.tapAsync('@aster-rpc/aster:gen', (_params, cb) => {
      try {
        generate(this.options);
        cb();
      } catch (e) {
        if (e instanceof ScanError) {
          const msg = `[aster-gen] ${e.message}`;
          if (this.options.warnOnly) {
            console.warn(msg);
            cb();
          } else {
            cb(new Error(msg));
          }
        } else {
          cb(e as Error);
        }
      }
    });
  }
}

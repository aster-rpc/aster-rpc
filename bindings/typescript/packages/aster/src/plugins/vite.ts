/**
 * Vite plugin for `aster-gen`.
 *
 * Runs the scanner at the start of each build (and, in `serve` mode,
 * at server startup + whenever a `@Service` / `@WireType`-decorated
 * source file changes). Users add it once in `vite.config.ts`:
 *
 * ```ts
 * import { defineConfig } from 'vite';
 * import { asterGen } from '@aster-rpc/aster/vite-plugin';
 *
 * export default defineConfig({
 *   plugins: [
 *     asterGen({
 *       project: 'tsconfig.json',
 *       out: 'aster-rpc.generated.ts',
 *     }),
 *   ],
 * });
 * ```
 *
 * The plugin is a thin wrapper around the programmatic
 * {@link generate} entry point in `cli/gen.ts` — all the real work
 * (AST walk, type mapping, emission) lives there. The plugin only
 * knows about the Vite lifecycle.
 */

import * as path from 'node:path';
import { generate, ScanError, type GenerateOptions } from '../cli/gen.js';

/**
 * Minimal subset of the Vite `Plugin` shape we need. We don't
 * import `vite` directly so users can use this plugin without
 * taking a hard dependency on Vite's types — helpful when the
 * plugin is imported from a non-Vite build (e.g. custom scripts).
 */
export interface VitePluginLike {
  name: string;
  enforce?: 'pre' | 'post';
  buildStart(): void | Promise<void>;
  handleHotUpdate?(ctx: { file: string }): void | Promise<void>;
}

export interface AsterGenViteOptions extends GenerateOptions {
  /**
   * Treat scan errors as warnings instead of failing the build.
   * Default: false (errors break the build — consistent with
   * other codegen tools). Set to true in dev mode if you want
   * HMR to keep working while you resolve a type issue.
   */
  warnOnly?: boolean;
}

/**
 * Vite plugin factory.
 *
 * Run order: `enforce: 'pre'` so the generated file exists before
 * any user import resolves it. The plugin regenerates on every
 * `buildStart`, plus on HMR when the changed file looks like it
 * might contain a decorator (cheap heuristic — rerun-on-any-.ts is
 * also acceptable since the scanner is fast).
 */
export function asterGen(options: AsterGenViteOptions): VitePluginLike {
  const run = (): void => {
    try {
      generate(options);
    } catch (e) {
      if (e instanceof ScanError) {
        const msg = `[aster-gen] ${e.message}`;
        if (options.warnOnly) {
          console.warn(msg);
        } else {
          throw new Error(msg);
        }
      } else {
        throw e;
      }
    }
  };

  const outAbs = path.resolve(options.out);
  return {
    name: '@aster-rpc/aster:gen',
    enforce: 'pre',
    buildStart() {
      run();
    },
    handleHotUpdate(ctx) {
      // Avoid loops: don't react to our own output.
      if (path.resolve(ctx.file) === outAbs) return;
      if (!/\.tsx?$/.test(ctx.file)) return;
      run();
    },
  };
}

export default asterGen;

/**
 * @aster-rpc/transport — type stubs for the native NAPI binding.
 *
 * The native binding is loaded dynamically at runtime by `index.js`. This
 * file declares its surface as `any` so the package can be imported from
 * TypeScript without compile errors.
 *
 * Consumers of `@aster-rpc/aster` never import from this package directly —
 * they go through the high-level API which has full types of its own.
 * Power users who reach into the raw NAPI surface should pin their
 * expectations against the Rust source in `bindings/typescript/native/src/`.
 */

declare const nativeBinding: any;
export = nativeBinding;

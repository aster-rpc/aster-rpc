#!/usr/bin/env node
/**
 * `aster-gen` — build-time scanner for `@aster-rpc/aster` services.
 *
 * Reads a TypeScript project via the TS compiler API, finds every
 * class decorated with `@Service` / `@WireType` from `@aster-rpc/aster`,
 * and emits an `aster-rpc.generated.ts` file that exports `SERVICES` and
 * `WIRE_TYPES` literals, auto-imported by `AsterServer.start()`.
 *
 * See `ffi_spec/ts-buildtime-audit.md` for the design notes and
 * `ffi_spec/Aster-ContractIdentity.md` §11.3.2.3 for the authoritative
 * TS type → wire type mapping.
 *
 * Usage:
 * ```
 *   npx aster-gen                           # defaults: ./tsconfig.json, ./aster-rpc.generated.ts
 *   npx aster-gen -p tsconfig.app.json
 *   npx aster-gen -o build/aster-rpc.generated.ts
 * ```
 */

import * as path from 'node:path';
import * as fs from 'node:fs';
import { createRequire } from 'node:module';
import ts from 'typescript';

// ─── CLI argv ────────────────────────────────────────────────────────────────

interface Cli {
  project: string;
  out: string;
  verbose: boolean;
}

function parseArgv(argv: string[]): Cli {
  const cli: Cli = {
    project: 'tsconfig.json',
    out: 'aster-rpc.generated.ts',
    verbose: false,
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '-p' || a === '--project') {
      cli.project = argv[++i] ?? cli.project;
    } else if (a === '-o' || a === '--out') {
      cli.out = argv[++i] ?? cli.out;
    } else if (a === '-v' || a === '--verbose') {
      cli.verbose = true;
    } else if (a === '-h' || a === '--help') {
      printHelp();
      process.exit(0);
    } else {
      console.error(`aster-gen: unknown argument: ${a}`);
      printHelp();
      process.exit(2);
    }
  }
  return cli;
}

function printHelp(): void {
  process.stdout.write(
    [
      'aster-gen — build-time scanner for @aster-rpc/aster',
      '',
      'Usage: aster-gen [options]',
      '',
      'Options:',
      '  -p, --project <tsconfig>   Path to tsconfig.json  (default: ./tsconfig.json)',
      '  -o, --out <file>           Output file            (default: ./aster-rpc.generated.ts)',
      '  -v, --verbose              Print discovered classes',
      '  -h, --help                 Show this help',
      '',
    ].join('\n'),
  );
}

// ─── Diagnostics ─────────────────────────────────────────────────────────────

class ScanError extends Error {
  constructor(msg: string, public loc?: string) {
    super(loc ? `${loc}: ${msg}` : msg);
  }
}

function locationOf(node: ts.Node): string {
  const sf = node.getSourceFile();
  if (!sf) return '<unknown>';
  const { line, character } = sf.getLineAndCharacterOfPosition(node.getStart());
  return `${sf.fileName}:${line + 1}:${character + 1}`;
}

// ─── Decorator identification ────────────────────────────────────────────────

/**
 * Decorator detection is name-only: any class whose decorator
 * identifier matches `Service` / `Rpc` / `WireType` / `ServerStream`
 * / `ClientStream` / `BidiStream` is treated as ours. This is robust
 * to how the user imports the package (installed, path alias, direct
 * source reference). A user with their own `Service` decorator would
 * need to alias the import or rename — the scanner errors loudly on
 * unresolved types, so collisions never corrupt output silently.
 */
function findDecorator(
  node: ts.HasDecorators,
  names: readonly string[],
): ts.Decorator | undefined {
  const decs = ts.getDecorators(node) ?? [];
  for (const d of decs) {
    const dn = decoratorName(d);
    if (dn && names.includes(dn)) return d;
  }
  return undefined;
}

function decoratorName(d: ts.Decorator): string | undefined {
  const expr = d.expression;
  const idExpr = ts.isCallExpression(expr) ? expr.expression : expr;
  if (!ts.isIdentifier(idExpr)) return undefined;
  return idExpr.text;
}

/**
 * Extract the first argument of a decorator call as an object literal,
 * returning a plain-object view of its properties. Only handles
 * primitive values (string / number / bool) and array-of-string —
 * anything more complex triggers a scan error at the call site.
 */
function readDecoratorOptions(
  decorator: ts.Decorator,
): Record<string, unknown> | undefined {
  if (!ts.isCallExpression(decorator.expression)) return undefined;
  const arg = decorator.expression.arguments[0];
  if (!arg) return undefined;
  if (!ts.isObjectLiteralExpression(arg)) {
    throw new ScanError(
      'decorator argument must be an object literal',
      locationOf(arg),
    );
  }
  const out: Record<string, unknown> = {};
  for (const prop of arg.properties) {
    if (!ts.isPropertyAssignment(prop)) continue;
    if (!ts.isIdentifier(prop.name) && !ts.isStringLiteral(prop.name)) continue;
    const key = prop.name.text;
    out[key] = literalValue(prop.initializer);
  }
  return out;
}

function literalValue(expr: ts.Expression): unknown {
  if (ts.isStringLiteral(expr) || ts.isNoSubstitutionTemplateLiteral(expr)) return expr.text;
  if (ts.isNumericLiteral(expr)) return Number(expr.text);
  if (expr.kind === ts.SyntaxKind.TrueKeyword) return true;
  if (expr.kind === ts.SyntaxKind.FalseKeyword) return false;
  if (ts.isArrayLiteralExpression(expr)) {
    return expr.elements.map(literalValue);
  }
  return undefined;
}

// ─── Type → WireFieldShape mapping ───────────────────────────────────────────

/**
 * Canonical wire-type tokens that can appear on a `@WireType` field.
 * Kept in sync with `src/generated.ts`.
 */
type ScanField =
  | { name: string; kind: 'primitive'; wire: string; nullable: boolean }
  | { name: string; kind: 'ref'; refTag: string; nullable: boolean }
  | { name: string; kind: 'list'; element: ScanField; nullable: boolean }
  | { name: string; kind: 'set'; element: ScanField; nullable: boolean }
  | { name: string; kind: 'map'; key: ScanField; value: ScanField; nullable: boolean };

/** Maps a brand tag (from `src/brand.ts`) to its wire primitive. */
const BRAND_WIRE: Record<string, string> = {
  i8: 'int8', i16: 'int16', i32: 'int32', i64: 'int64',
  u8: 'uint8', u16: 'uint16', u32: 'uint32', u64: 'uint64',
  f32: 'float32', f64: 'float64',
};

/** Maps TypeScript primitive type names to their Aster wire primitive. */
const PRIMITIVE_WIRE: Record<string, string> = {
  string: 'string',
  number: 'float64',
  boolean: 'bool',
  int8: 'int8', int16: 'int16', int32: 'int32', int64: 'int64',
  uint8: 'uint8', uint16: 'uint16', uint32: 'uint32', uint64: 'uint64',
  Float32: 'float32', Float64: 'float64',
  undefined: 'string', // fallback for unknown
};

/** Spec §11.3.2.3 suspicious-field regex for `number` fields. */
const SUSPICIOUS_NUMBER_FIELD_RE =
  /(count|id|size|length|index|offset|timestamp|epoch|nanos|micros|millis|seconds|bytes|total|version)/i;

interface ScanContext {
  checker: ts.TypeChecker;
  wireTypeTags: Map<ts.Symbol, string>;
  warnings: string[];
}

/**
 * Peel `null` / `undefined` / optional members off a union type.
 * Returns the non-nullable inner type and whether the original was
 * nullable. `field?: T`, `T | null`, `T | undefined`, and combinations
 * all collapse to the same `nullable<T>` wire type (spec §11.3.2.3).
 */
function peelNullable(t: ts.Type): { inner: ts.Type; nullable: boolean } {
  if (!t.isUnion()) return { inner: t, nullable: false };
  const keep: ts.Type[] = [];
  let nullable = false;
  for (const u of t.types) {
    if (u.flags & (ts.TypeFlags.Null | ts.TypeFlags.Undefined | ts.TypeFlags.Void)) {
      nullable = true;
      continue;
    }
    keep.push(u);
  }
  if (!nullable) return { inner: t, nullable: false };
  if (keep.length === 1) return { inner: keep[0]!, nullable: true };
  // Union of multiple real types — not yet supported
  return { inner: t, nullable: true };
}

/**
 * Recognize a branded primitive like `i32`, `f64`. The brand shape is
 * `base & { readonly __asterBrand: '<tag>' }`, see `src/brand.ts`.
 *
 * We walk intersection types manually because aliased intersections
 * (`i64 = bigint & {...}`) don't expose merged properties via the
 * high-level `t.getProperty` path reliably — the alias is preserved.
 */
function readBrand(checker: ts.TypeChecker, t: ts.Type): string | undefined {
  const candidates: ts.Type[] = t.isIntersection() ? [...t.types, t] : [t];
  for (const c of candidates) {
    const prop = checker.getPropertyOfType(c, '__asterBrand');
    if (!prop) continue;
    const decl = prop.valueDeclaration ?? prop.declarations?.[0];
    if (!decl) continue;
    const brandType = checker.getTypeOfSymbolAtLocation(prop, decl);
    if (brandType.isStringLiteral()) return brandType.value;
  }
  return undefined;
}

/** Convert a `ts.Type` into a wire field shape. */
function typeToField(
  ctx: ScanContext,
  name: string,
  rawType: ts.Type,
  loc: string,
): ScanField {
  const { inner, nullable } = peelNullable(rawType);
  const base = typeToFieldInner(ctx, name, inner, loc);
  return { ...base, nullable: nullable || base.nullable } as ScanField;
}

function typeToFieldInner(
  ctx: ScanContext,
  name: string,
  t: ts.Type,
  loc: string,
): ScanField {
  const checker = ctx.checker;

  // Brand detection runs before the coarse primitive check: a branded
  // `number & {__asterBrand: 'i32'}` has TypeFlags.Number set but we
  // want to preserve the brand's wire token.
  const brand = readBrand(checker, t);
  if (brand) {
    const wire = BRAND_WIRE[brand];
    if (!wire) throw new ScanError(`unknown aster brand '${brand}' on field '${name}'`, loc);
    return { name, kind: 'primitive', wire, nullable: false };
  }

  // Primitives
  if (t.flags & ts.TypeFlags.BooleanLike) {
    return { name, kind: 'primitive', wire: 'bool', nullable: false };
  }
  if (t.flags & ts.TypeFlags.NumberLike) {
    if (SUSPICIOUS_NUMBER_FIELD_RE.test(name)) {
      ctx.warnings.push(
        `${loc}: field '${name}' is 'number' — did you mean 'bigint' / aster.i64? ` +
        `Field name matches the suspicious-identifier list (spec §11.3.2.3).`,
      );
    }
    return { name, kind: 'primitive', wire: 'float64', nullable: false };
  }
  if (t.flags & ts.TypeFlags.BigIntLike) {
    return { name, kind: 'primitive', wire: 'int64', nullable: false };
  }
  if (t.flags & ts.TypeFlags.StringLike) {
    return { name, kind: 'primitive', wire: 'string', nullable: false };
  }

  // Object types — arrays, maps, sets, Date, Uint8Array, @WireType refs
  const sym = t.getSymbol();
  const symName = sym?.getName();

  if (symName === 'Uint8Array') {
    return { name, kind: 'primitive', wire: 'binary', nullable: false };
  }
  if (symName === 'Date') {
    return { name, kind: 'primitive', wire: 'timestamp', nullable: false };
  }

  // Homogeneous tuple types readonly [T, T, ...] also show up as tuple
  // types here; the spec forbids heterogeneous tuples, so flag them.
  if (checker.isTupleType(t)) {
    const tupleTypes = checker.getTypeArguments(t as ts.TypeReference);
    if (tupleTypes.length === 0) {
      throw new ScanError(`empty tuple field '${name}' is not representable`, loc);
    }
    const first = tupleTypes[0]!;
    for (const tt of tupleTypes) {
      if (!checker.isTypeAssignableTo(tt, first) || !checker.isTypeAssignableTo(first, tt)) {
        throw new ScanError(
          `heterogeneous tuple field '${name}' is forbidden (spec §11.3.2.3). ` +
          `Use a named message type or a homogeneous list.`,
          loc,
        );
      }
    }
    const element = typeToField(ctx, `${name}[]`, first, loc);
    return { name, kind: 'list', element, nullable: false };
  }

  if (symName === 'Array' || symName === 'ReadonlyArray') {
    const args = checker.getTypeArguments(t as ts.TypeReference);
    if (args.length !== 1) {
      throw new ScanError(`Array field '${name}' must have a type argument`, loc);
    }
    const element = typeToField(ctx, `${name}[]`, args[0]!, loc);
    return { name, kind: 'list', element, nullable: false };
  }

  if (symName === 'Set' || symName === 'ReadonlySet') {
    const args = checker.getTypeArguments(t as ts.TypeReference);
    if (args.length !== 1) {
      throw new ScanError(`Set field '${name}' must have a type argument`, loc);
    }
    const element = typeToField(ctx, `${name}[]`, args[0]!, loc);
    return { name, kind: 'set', element, nullable: false };
  }

  if (symName === 'Map' || symName === 'ReadonlyMap') {
    const args = checker.getTypeArguments(t as ts.TypeReference);
    if (args.length !== 2) {
      throw new ScanError(`Map field '${name}' must have two type arguments`, loc);
    }
    const key = typeToField(ctx, `${name}[key]`, args[0]!, loc);
    const value = typeToField(ctx, `${name}[value]`, args[1]!, loc);
    return { name, kind: 'map', key, value, nullable: false };
  }

  // Record<string, V> — treated as map with string keys. Common TypeScript
  // idiom for dictionaries; the spec's restriction on map keys (string or
  // number only) is satisfied by Record's key constraint.
  // Record is a type alias so getTypeArguments returns [] after resolution.
  // Parse the type string (e.g. "Record<string, string>") to get args.
  const typeStr = checker.typeToString(t);
  if (typeStr.startsWith('Record<') && typeStr.endsWith('>')) {
    const inner = typeStr.slice(7, -1); // "string, string"
    const commaIdx = inner.indexOf(',');
    if (commaIdx === -1) {
      throw new ScanError(`Record field '${name}' must have two type arguments`, loc);
    }
    const keyStr = inner.slice(0, commaIdx).trim();
    const valueStr = inner.slice(commaIdx + 1).trim();
    // Build key field: only string is valid for Record keys
    if (keyStr !== 'string') {
      throw new ScanError(
        `Record field '${name}' has non-string key '${keyStr}'. ` +
        `Only Record<string, V> is supported (V must be a wire primitive).`,
        loc,
      );
    }
    // Build value field from the primitive type string
    const valuePrimitive = PRIMITIVE_WIRE[valueStr];
    if (!valuePrimitive) {
      throw new ScanError(
        `Record field '${name}' value type '${valueStr}' is not a wire primitive. ` +
        `Use Map<K, V> for complex value types.`,
        loc,
      );
    }
    const keyField: ScanField = { name: `${name}[key]`, kind: 'primitive', wire: 'string', nullable: false };
    const valueField: ScanField = { name: `${name}[value]`, kind: 'primitive', wire: valuePrimitive, nullable: false };
    return { name, kind: 'map', key: keyField, value: valueField, nullable: false };
  }

  // @WireType class reference
  if (sym && ctx.wireTypeTags.has(sym)) {
    const tag = ctx.wireTypeTags.get(sym)!;
    return { name, kind: 'ref', refTag: tag, nullable: false };
  }

  // Unknown / unsupported
  const pretty = checker.typeToString(t);
  throw new ScanError(
    `field '${name}' has unsupported type '${pretty}'. ` +
    `Expected a primitive, a @WireType class, or Array/Map/Set/Date/Uint8Array. ` +
    `(spec §11.3.2.3)`,
    loc,
  );
}

// ─── Scanning phase ──────────────────────────────────────────────────────────

interface ScannedWireType {
  sym: ts.Symbol;
  classDecl: ts.ClassDeclaration;
  tag: string;
  fields: ScanField[];
  sourceFile: string;
  importName: string;
}

interface ScannedMethod {
  name: string;
  pattern: 'unary' | 'server_stream' | 'client_stream' | 'bidi_stream';
  requestTypeSym: ts.Symbol | undefined;
  responseTypeSym: ts.Symbol | undefined;
  acceptsCtx: boolean;
  idempotent: boolean;
  timeout: number | undefined;
}

interface ScannedService {
  sym: ts.Symbol;
  classDecl: ts.ClassDeclaration;
  name: string;
  version: number;
  scoped: 'shared' | 'session';
  serializationModes: string[];
  methods: ScannedMethod[];
  sourceFile: string;
  importName: string;
}

const METHOD_DECORATORS: Record<string, ScannedMethod['pattern']> = {
  Rpc: 'unary',
  ServerStream: 'server_stream',
  ClientStream: 'client_stream',
  BidiStream: 'bidi_stream',
};

function scanWireType(
  ctx: ScanContext,
  classDecl: ts.ClassDeclaration,
  decorator: ts.Decorator,
): ScannedWireType {
  if (!ts.isCallExpression(decorator.expression)) {
    throw new ScanError('@WireType must be called with a tag', locationOf(decorator));
  }
  const args = decorator.expression.arguments;
  const tagArg = args[0];
  if (!tagArg || !ts.isStringLiteralLike(tagArg)) {
    throw new ScanError('@WireType tag must be a string literal', locationOf(decorator));
  }
  const tag = tagArg.text;

  const sym = ctx.checker.getSymbolAtLocation(classDecl.name!)!;
  const classType = ctx.checker.getTypeAtLocation(classDecl);
  const props = ctx.checker.getPropertiesOfType(classType);

  const fields: ScanField[] = [];
  for (const prop of props) {
    // Skip methods and symbol-keyed properties
    const decl = prop.valueDeclaration;
    if (!decl) continue;
    if (!ts.isPropertyDeclaration(decl)) continue;
    if (decl.modifiers?.some(m => m.kind === ts.SyntaxKind.StaticKeyword)) continue;

    const propType = ctx.checker.getTypeOfSymbolAtLocation(prop, decl);
    const field = typeToField(ctx, prop.getName(), propType, locationOf(decl));
    fields.push(field);
  }

  return {
    sym,
    classDecl,
    tag,
    fields,
    sourceFile: classDecl.getSourceFile().fileName,
    importName: classDecl.name!.text,
  };
}

function scanService(
  ctx: ScanContext,
  classDecl: ts.ClassDeclaration,
  decorator: ts.Decorator,
): ScannedService {
  const opts = readDecoratorOptions(decorator) ?? {};
  const name = typeof opts.name === 'string' ? opts.name : classDecl.name?.text;
  if (!name) {
    throw new ScanError('@Service requires a { name } option', locationOf(decorator));
  }
  const version = typeof opts.version === 'number' ? opts.version : 1;
  const scopedRaw = typeof opts.scoped === 'string' ? opts.scoped : 'shared';
  const scoped = (scopedRaw === 'session' || scopedRaw === 'stream') ? 'session' : 'shared';
  const serializationModes = Array.isArray(opts.serialization)
    ? (opts.serialization as unknown[]).filter((s): s is string => typeof s === 'string')
    : [];

  const sym = ctx.checker.getSymbolAtLocation(classDecl.name!)!;
  const methods: ScannedMethod[] = [];

  for (const member of classDecl.members) {
    if (!ts.isMethodDeclaration(member)) continue;
    if (!member.name || !ts.isIdentifier(member.name)) continue;
    const decs = ts.getDecorators(member) ?? [];
    let methodDecorator: ts.Decorator | undefined;
    let pattern: ScannedMethod['pattern'] | undefined;
    for (const d of decs) {
      const dn = decoratorName(d);
      if (!dn) continue;
      const p = METHOD_DECORATORS[dn];
      if (p) {
        methodDecorator = d;
        pattern = p;
        break;
      }
    }
    if (!methodDecorator || !pattern) continue;

    const methodOpts = readDecoratorOptions(methodDecorator) ?? {};
    const methodName = typeof methodOpts.name === 'string' ? methodOpts.name : member.name.text;
    const timeout = typeof methodOpts.timeout === 'number' ? methodOpts.timeout : undefined;
    const idempotent = methodOpts.idempotent === true;

    // First non-CallContext parameter is the request; second (if present
    // and typed as CallContext) sets acceptsCtx.
    const params = member.parameters;
    let requestTypeSym: ts.Symbol | undefined;
    let responseTypeSym: ts.Symbol | undefined;
    let acceptsCtx = false;

    if (params.length > 0) {
      const reqParam = params[0]!;
      const reqType = ctx.checker.getTypeAtLocation(reqParam);
      const reqSym = reqType.getSymbol();
      if (reqSym) requestTypeSym = reqSym;
    }
    if (params.length > 1) {
      const ctxParam = params[1]!;
      const ctxType = ctx.checker.getTypeAtLocation(ctxParam);
      const ctxSym = ctxType.getSymbol();
      if (ctxSym?.getName() === 'CallContext') {
        acceptsCtx = true;
      }
    }

    // Response type: return type unwrapped from Promise<T> / AsyncGenerator<T>
    const signature = ctx.checker.getSignatureFromDeclaration(member);
    if (signature) {
      const ret = ctx.checker.getReturnTypeOfSignature(signature);
      const unwrapped = unwrapAsyncReturn(ctx.checker, ret);
      responseTypeSym = unwrapped?.getSymbol();
    }

    methods.push({
      name: methodName,
      pattern,
      requestTypeSym,
      responseTypeSym,
      acceptsCtx,
      idempotent,
      timeout,
    });
  }

  return {
    sym,
    classDecl,
    name,
    version,
    scoped,
    serializationModes,
    methods,
    sourceFile: classDecl.getSourceFile().fileName,
    importName: classDecl.name!.text,
  };
}

function unwrapAsyncReturn(checker: ts.TypeChecker, t: ts.Type): ts.Type | undefined {
  const sym = t.getSymbol();
  const symName = sym?.getName();
  if (symName === 'Promise' || symName === 'AsyncGenerator' || symName === 'AsyncIterableIterator') {
    const args = checker.getTypeArguments(t as ts.TypeReference);
    if (args.length >= 1) return args[0];
  }
  return t;
}

// ─── Dependency ordering (Tarjan SCC) ────────────────────────────────────────

/**
 * Collect all wire-type tags referenced (transitively through containers)
 * by a single scanned field. Used for building the reference graph and
 * for back-edge detection within SCCs.
 */
function collectFieldRefs(f: ScanField, acc: Set<string>): void {
  if (f.kind === 'ref') {
    acc.add(f.refTag);
  } else if (f.kind === 'list' || f.kind === 'set') {
    collectFieldRefs(f.element, acc);
  } else if (f.kind === 'map') {
    collectFieldRefs(f.key, acc);
    collectFieldRefs(f.value, acc);
  }
}

function wireTypeDeps(w: ScannedWireType): Set<string> {
  const acc = new Set<string>();
  for (const f of w.fields) collectFieldRefs(f, acc);
  return acc;
}

/**
 * Tarjan's SCC on the @WireType reference graph, keyed by wire tag.
 * Returns SCCs in reverse topological order (leaves first) — ready for
 * bottom-up hashing. Self-referential types (`Entry.children: Entry[]`)
 * land in a single-node SCC with a self-edge; mutually recursive types
 * (`A ↔ B`) land in a multi-node SCC. The caller handles back-edge
 * detection and SELF_REF emission.
 *
 * Spec: `Aster-ContractIdentity.md` §11.3. Mirror of the Rust
 * implementation in `core/src/contract.rs::tarjan_scc`.
 */
function tarjanSccs(wireTypes: ScannedWireType[]): ScannedWireType[][] {
  const byTag = new Map<string, ScannedWireType>();
  for (const w of wireTypes) byTag.set(w.tag, w);

  let idx = 0;
  const index = new Map<string, number>();
  const lowlink = new Map<string, number>();
  const onStack = new Set<string>();
  const stack: string[] = [];
  const sccs: ScannedWireType[][] = [];

  function strongconnect(vTag: string): void {
    index.set(vTag, idx);
    lowlink.set(vTag, idx);
    idx++;
    stack.push(vTag);
    onStack.add(vTag);

    const v = byTag.get(vTag);
    if (v) {
      // Sort deps for deterministic output across runs.
      const deps = [...wireTypeDeps(v)].filter(t => byTag.has(t)).sort();
      for (const wTag of deps) {
        if (!index.has(wTag)) {
          strongconnect(wTag);
          lowlink.set(vTag, Math.min(lowlink.get(vTag)!, lowlink.get(wTag)!));
        } else if (onStack.has(wTag)) {
          lowlink.set(vTag, Math.min(lowlink.get(vTag)!, index.get(wTag)!));
        }
      }
    }

    if (lowlink.get(vTag) === index.get(vTag)) {
      const scc: ScannedWireType[] = [];
      while (true) {
        const u = stack.pop()!;
        onStack.delete(u);
        const uw = byTag.get(u);
        if (uw) scc.push(uw);
        if (u === vTag) break;
      }
      sccs.push(scc);
    }
  }

  // Visit in deterministic tag order so SCC output is stable.
  const roots = [...wireTypes].map(w => w.tag).sort();
  for (const t of roots) {
    if (!index.has(t)) strongconnect(t);
  }
  return sccs;
}

/**
 * DFS within a multi-node SCC from a fixed start node, recording which
 * edges are back-edges (target already visited on the DFS path). Edges
 * in the spanning tree resolve to REF via the already-computed hashes;
 * back-edges resolve to SELF_REF so the cycle can be broken without a
 * fixed-point hash iteration. Mirrors Python's `_spanning_tree_dfs` in
 * `identity.py`.
 */
function sccBackEdges(scc: ScannedWireType[]): Map<string, Set<string>> {
  const result = new Map<string, Set<string>>();
  if (scc.length === 1) {
    const w = scc[0]!;
    const deps = wireTypeDeps(w);
    if (deps.has(w.tag)) {
      result.set(w.tag, new Set([w.tag]));
    }
    return result;
  }

  // Multi-node SCC: DFS from the lexicographically smallest tag (matches
  // Python's NFC-sorted choice of start node — deterministic and
  // language-independent).
  const memberTags = new Set(scc.map(w => w.tag));
  const byTag = new Map<string, ScannedWireType>();
  for (const w of scc) byTag.set(w.tag, w);
  const start = [...memberTags].sort()[0]!;

  const visited = new Set<string>();
  function dfs(tag: string): void {
    visited.add(tag);
    const w = byTag.get(tag);
    if (!w) return;
    const deps = [...wireTypeDeps(w)].filter(t => memberTags.has(t)).sort();
    for (const r of deps) {
      if (!visited.has(r)) {
        dfs(r);
      } else {
        if (!result.has(tag)) result.set(tag, new Set());
        result.get(tag)!.add(r);
      }
    }
  }
  dfs(start);
  return result;
}

// ─── Type hash computation ───────────────────────────────────────────────────

/**
 * Rust `FieldDef` JSON shape — must match `core/src/contract.rs::FieldDef`
 * serde layout exactly. Field names are the serde-default snake_case of
 * the struct, all fields present (serde defaults still parse, but we
 * emit the full shape for clarity).
 */
interface FieldDefJson {
  id: number;
  name: string;
  type_kind: 'primitive' | 'ref' | 'self_ref' | 'any';
  type_primitive: string;
  type_ref: string; // hex-encoded bytes (64 chars for a 32-byte hash)
  self_ref_name: string;
  optional: boolean;
  ref_tracked: boolean;
  container: 'none' | 'list' | 'set' | 'map';
  container_key_kind: 'primitive' | 'ref' | 'self_ref' | 'any';
  container_key_primitive: string;
  container_key_ref: string;
  required: boolean;
  default_value: string;
}

interface TypeDefJson {
  kind: 'message' | 'enum' | 'union';
  package: string;
  name: string;
  fields: FieldDefJson[];
  enum_values: never[];
  union_variants: never[];
}

/** Lazy-loaded NAPI binding — same module the runtime uses. */
interface NativeContractBinding {
  canonicalBytesFromJson(typeName: string, json: string): Uint8Array;
  computeTypeHash(data: Uint8Array): Uint8Array;
}

let _nativeBinding: NativeContractBinding | undefined;
function loadNativeBinding(): NativeContractBinding {
  if (_nativeBinding) return _nativeBinding;
  // `@aster-rpc/aster` is ESM, but the NAPI addon is CJS. `createRequire`
  // gives us a sync-loading CJS require resolved from this module — the
  // native addon is loaded once and cached.
  const req = createRequire(import.meta.url);
  const mod = req('@aster-rpc/transport');
  if (typeof mod.canonicalBytesFromJson !== 'function' ||
      typeof mod.computeTypeHash !== 'function') {
    throw new ScanError(
      'aster-gen: @aster-rpc/transport is missing canonicalBytesFromJson / ' +
      'computeTypeHash. Rebuild the native addon: cd bindings/typescript/native && yarn build',
    );
  }
  _nativeBinding = mod as NativeContractBinding;
  return _nativeBinding;
}

function bytesToHex(bytes: Uint8Array): string {
  let s = '';
  for (const b of bytes) s += b.toString(16).padStart(2, '0');
  return s;
}

interface LeafShape {
  type_kind: FieldDefJson['type_kind'];
  type_primitive: string;
  type_ref: string;
  self_ref_name: string;
}

/**
 * Map a ScanField primitive leaf to a Rust FieldDef leaf shape.
 * `backEdgeTargets` is the set of wire tags (within the current SCC)
 * that should emit `type_kind: self_ref` instead of `ref` — this is
 * how cyclic types break the hash chicken-and-egg. For acyclic types
 * the set is empty and everything resolves to `ref` via `typeHashes`.
 */
function resolveLeaf(
  f: ScanField,
  typeHashes: Map<string, string>,
  backEdgeTargets: Set<string>,
): LeafShape {
  if (f.kind === 'primitive') {
    return { type_kind: 'primitive', type_primitive: f.wire, type_ref: '', self_ref_name: '' };
  }
  if (f.kind === 'ref') {
    if (backEdgeTargets.has(f.refTag)) {
      // SELF_REF: encode the target by wire tag rather than by hash.
      // The tag is language-neutral (unlike Python's current use of
      // __module__.__qualname__, which is a latent cross-language
      // parity gap on the Python side).
      return { type_kind: 'self_ref', type_primitive: '', type_ref: '', self_ref_name: f.refTag };
    }
    const hash = typeHashes.get(f.refTag);
    if (!hash) {
      throw new ScanError(
        `aster-gen: unresolved @WireType reference '${f.refTag}' — ` +
        `Tarjan SCC should have hashed it first. This is a bug in ` +
        `tarjanSccs or computeTypeHashes.`,
      );
    }
    return { type_kind: 'ref', type_primitive: '', type_ref: hash, self_ref_name: '' };
  }
  // Containers can't be leaves — scanFieldToFieldDef peels them first.
  throw new ScanError(
    `aster-gen: nested container in field '${f.name}' is not representable ` +
    `in a single Rust FieldDef. Spec §11.3.2.3 allows at most one container ` +
    `level per field.`,
  );
}

/**
 * Convert a ScanField to the Rust FieldDef JSON shape. Handles primitives,
 * refs, and one level of list / set / map container (matching Python's
 * `_build_field_def_for`).
 */
function scanFieldToFieldDef(
  f: ScanField,
  index: number,
  typeHashes: Map<string, string>,
  backEdgeTargets: Set<string>,
): FieldDefJson {
  const base: FieldDefJson = {
    id: index,
    name: f.name,
    type_kind: 'primitive',
    type_primitive: '',
    type_ref: '',
    self_ref_name: '',
    optional: f.nullable,
    ref_tracked: false,
    container: 'none',
    container_key_kind: 'primitive',
    container_key_primitive: '',
    container_key_ref: '',
    required: true,
    default_value: '',
  };

  if (f.kind === 'primitive' || f.kind === 'ref') {
    const leaf = resolveLeaf(f, typeHashes, backEdgeTargets);
    return { ...base, ...leaf };
  }

  if (f.kind === 'list' || f.kind === 'set') {
    const leaf = resolveLeaf(f.element, typeHashes, backEdgeTargets);
    return { ...base, container: f.kind, ...leaf };
  }

  // map
  const valueLeaf = resolveLeaf(f.value, typeHashes, backEdgeTargets);
  const keyLeaf = resolveLeaf(f.key, typeHashes, backEdgeTargets);
  return {
    ...base,
    container: 'map',
    ...valueLeaf,
    container_key_kind: keyLeaf.type_kind,
    container_key_primitive: keyLeaf.type_primitive,
    container_key_ref: keyLeaf.type_ref,
  };
}

/**
 * Split a @WireType tag into (package, name) using the conventional
 * `"namespace/TypeName"` form. A tag without a `/` is treated as an
 * unnamespaced type. This must match how Python's `@wire_type` populates
 * `__fory_namespace__` / `__fory_typename__` so TS and Python compute
 * identical TypeDef bytes for the same logical type.
 */
function splitTag(tag: string): { package: string; name: string } {
  const idx = tag.lastIndexOf('/');
  if (idx < 0) return { package: '', name: tag };
  return { package: tag.slice(0, idx), name: tag.slice(idx + 1) };
}

function wireTypeToTypeDef(
  w: ScannedWireType,
  typeHashes: Map<string, string>,
  backEdgeTargets: Set<string>,
): TypeDefJson {
  const { package: pkg, name } = splitTag(w.tag);
  const fields = w.fields.map((f, i) => scanFieldToFieldDef(f, i + 1, typeHashes, backEdgeTargets));
  return {
    kind: 'message',
    package: pkg,
    name,
    fields,
    enum_values: [],
    union_variants: [],
  };
}

/**
 * Walk wire types SCC-by-SCC in reverse topological order, compute each
 * type's canonical BLAKE3 hash via the native binding, and return both
 * the tag→hash map (input to method hash emission) and the flattened
 * ordered list (input to emission so the generated file keeps leaves-
 * first layout).
 *
 * Cyclic types are broken via `SELF_REF`: within an SCC, back-edges in
 * a spanning-tree DFS emit `type_kind: self_ref` with the target wire
 * tag in `self_ref_name`, instead of the (not-yet-computed) target
 * hash. This is the standard trick for content-hashing a Merkle DAG
 * over cyclic references; see `Aster-ContractIdentity.md` §11.3.2.2.
 */
function computeTypeHashes(wireTypes: ScannedWireType[]): {
  hashes: Map<string, string>;
  ordered: ScannedWireType[];
} {
  const native = loadNativeBinding();
  const sccs = tarjanSccs(wireTypes);
  const hashes = new Map<string, string>();
  const ordered: ScannedWireType[] = [];

  for (const scc of sccs) {
    const backEdgesBySource = sccBackEdges(scc);
    for (const w of scc) {
      const backEdgeTargets = backEdgesBySource.get(w.tag) ?? new Set<string>();
      const td = wireTypeToTypeDef(w, hashes, backEdgeTargets);
      const canonical = native.canonicalBytesFromJson('TypeDef', JSON.stringify(td));
      const hash = native.computeTypeHash(canonical);
      hashes.set(w.tag, bytesToHex(hash));
      ordered.push(w);
    }
  }
  return { hashes, ordered };
}

/**
 * Emit a Uint8Array literal expression from a hex-encoded 32-byte hash.
 * The generated form is `new Uint8Array([0xab, 0xcd, ...])` — terse
 * enough that the output stays readable, and zero-runtime-cost (no
 * `hex.decode` call at module load).
 */
function emitHashLiteral(hex: string): string {
  const parts: string[] = [];
  for (let i = 0; i < hex.length; i += 2) {
    parts.push('0x' + hex.substr(i, 2));
  }
  return `new Uint8Array([${parts.join(', ')}])`;
}

// ─── Output emission ─────────────────────────────────────────────────────────

/** Build an import map: source file → { imported name → local alias }. */
function buildImports(
  outDir: string,
  wireTypes: ScannedWireType[],
  services: ScannedService[],
): { header: string; aliasFor: Map<ts.Symbol, string> } {
  const bySource = new Map<string, Map<string, string>>();
  const aliasFor = new Map<ts.Symbol, string>();
  let counter = 0;

  function addImport(file: string, name: string, sym: ts.Symbol): string {
    if (aliasFor.has(sym)) return aliasFor.get(sym)!;
    const alias = `T${counter++}_${name}`;
    aliasFor.set(sym, alias);
    let group = bySource.get(file);
    if (!group) {
      group = new Map();
      bySource.set(file, group);
    }
    group.set(name, alias);
    return alias;
  }

  for (const w of wireTypes) addImport(w.sourceFile, w.importName, w.sym);
  for (const s of services) addImport(s.sourceFile, s.importName, s.sym);

  const lines: string[] = [];
  for (const [file, group] of bySource) {
    const rel = relImport(outDir, file);
    const imports = [...group.entries()].map(([n, a]) => `${n} as ${a}`).join(', ');
    lines.push(`import { ${imports} } from '${rel}';`);
  }
  return { header: lines.join('\n'), aliasFor };
}

function relImport(outFile: string, targetFile: string): string {
  const outDir = path.dirname(outFile);
  let rel = path.relative(outDir, targetFile).replace(/\\/g, '/');
  // Strip .ts extension and add .js for Node16 module resolution
  rel = rel.replace(/\.tsx?$/, '.js');
  if (!rel.startsWith('.')) rel = './' + rel;
  return rel;
}

function emitField(f: ScanField): string {
  const nullable = f.nullable ? ', nullable: true' : ', nullable: false';
  switch (f.kind) {
    case 'primitive':
      return `{ name: ${JSON.stringify(f.name)}, kind: 'primitive', wire: ${JSON.stringify(f.wire)}${nullable} }`;
    case 'ref':
      return `{ name: ${JSON.stringify(f.name)}, kind: 'ref', refTag: ${JSON.stringify(f.refTag)}${nullable} }`;
    case 'list':
      return `{ name: ${JSON.stringify(f.name)}, kind: 'list', element: ${emitField(f.element)}${nullable} }`;
    case 'set':
      return `{ name: ${JSON.stringify(f.name)}, kind: 'set', element: ${emitField(f.element)}${nullable} }`;
    case 'map':
      return `{ name: ${JSON.stringify(f.name)}, kind: 'map', key: ${emitField(f.key)}, value: ${emitField(f.value)}${nullable} }`;
  }
}

function emitWireTypes(
  wireTypes: ScannedWireType[],
  aliasFor: Map<ts.Symbol, string>,
): string {
  const entries: string[] = [];
  for (const w of wireTypes) {
    const alias = aliasFor.get(w.sym)!;
    const fieldExprs = w.fields.map(f => `    ${emitField(f)},`).join('\n');
    const fieldNames = w.fields.map(f => JSON.stringify(f.name)).join(', ');
    // Nested @WireType references for JSON shape validator recursion.
    const nestedEntries: string[] = [];
    const elementEntries: string[] = [];
    for (const f of w.fields) {
      if (f.kind === 'ref') {
        const ref = wireTypes.find(x => x.tag === f.refTag);
        if (ref) nestedEntries.push(`[${JSON.stringify(f.name)}, ${aliasFor.get(ref.sym)}]`);
      } else if (f.kind === 'list' || f.kind === 'set') {
        const el = f.element;
        if (el.kind === 'ref') {
          const ref = wireTypes.find(x => x.tag === el.refTag);
          if (ref) elementEntries.push(`[${JSON.stringify(f.name)}, ${aliasFor.get(ref.sym)}]`);
        }
      }
    }
    entries.push(
      `  {
    ctor: ${alias},
    tag: ${JSON.stringify(w.tag)},
    fields: [
${fieldExprs}
    ],
    fieldNameSet: new Set([${fieldNames}]),
    nestedTypes: new Map([${nestedEntries.join(', ')}]),
    elementTypes: new Map([${elementEntries.join(', ')}]),
  },`,
    );
  }

  // BUILD_ALL_TYPES: constructs and registers all Fory type structs using
  // Type.struct(), in dependency order (leaves first via topological sort).
  //
  // Iterates over WIRE_TYPES so that entry.ctor is the actual class ref
  // (not a string that needs Function() eval). For 'ref' fields, looks up
  // the target type from typesByTag by refTag — all deps are registered
  // before their referrers because wireTypes is topologically sorted.
  const buildAllTypesBody = wireTypes.map(w => {
    const fieldsCode = w.fields.map(f => {
      function fieldToTypeExpr(fld: typeof f): string {
        const wrap = fld.nullable ? 'Type.optional(' : '';
        const close = fld.nullable ? ')' : '';
        switch (fld.kind) {
          case 'primitive':
            return `${wrap}Type.${fld.wire}()${close}`;
          case 'ref':
            // typesByTag has the type registered when we reach this field.
            return `${wrap}typesByTag.get(${JSON.stringify(fld.refTag)})${close}`;
          case 'list':
            return `${wrap}Type.array(${fieldToTypeExpr(fld.element)})${close}`;
          case 'set':
            return `${wrap}Type.set(${fieldToTypeExpr(fld.element)})${close}`;
          case 'map':
            return `${wrap}Type.map(${fieldToTypeExpr(fld.key)}, ${fieldToTypeExpr(fld.value)})${close}`;
        }
      }
      return `      ${JSON.stringify(f.name)}: ${fieldToTypeExpr(f)}`;
    }).join(',\n');
    return `  // ${w.tag}
  {
    const [ns, typeName] = ${JSON.stringify(w.tag)}.split('/');
    const typeStruct = Type.struct(
      { namespace: ns, typeName },
      {
${fieldsCode}
      },
      { withConstructor: true },
    );
    // entry.ctor is the actual class constructor from WIRE_TYPES.
    // initMeta can only be called once per class (sets non-configurable property).
    // Skip if already initialized (prototype already has ForyTypeInfoSymbol set).
    const proto = entry.ctor.prototype;
    if (!proto.hasOwnProperty('__foryTypeInfoInit__')) {
      try {
        typeStruct.initMeta(entry.ctor);
        Object.defineProperty(proto, '__foryTypeInfoInit__', { value: true, configurable: true });
      } catch (e: any) {
        // already initialized — skip
      }
    }
    codec.registerType(typeStruct);
    typesByTag.set(${JSON.stringify(w.tag)}, typeStruct);
  }`;
  }).join('\n');

  return `export const WIRE_TYPES = [
${entries.join('\n')}
] as const;

/**
 * Build and register all @WireType classes with Fory, using Type.struct()
 * to describe each struct's fields with explicit wire types.
 *
 * Iterates over WIRE_TYPES so that entry.ctor is the actual class ref.
 * For 'ref' fields, looks up the target type from typesByTag by refTag.
 * Types are registered in topological order (leaves first), so all
 * dependencies are available when any given type is being registered.
 *
 * @returns Map<tag, typeStruct> — each registered Fory type struct, keyed
 *   by wire tag (e.g. "sample/StatusRequest"). Callers can use this to
 *   look up any type after registration.
 */
export function BUILD_ALL_TYPES(
  fory: any,
  Type: any,
  codec: { registerType(typeInfo: any): void },
): Map<string, any> {
  const typesByTag = new Map();
  for (const entry of WIRE_TYPES) {
${buildAllTypesBody}
  }
  return typesByTag;
}
`;
}


function emitServices(
  wireTypes: ScannedWireType[],
  services: ScannedService[],
  aliasFor: Map<ts.Symbol, string>,
  typeHashes: Map<string, string>,
): string {
  // Map a method's request/response type symbol to the @WireType tag,
  // so we can look up the precomputed hash.
  const tagBySym = new Map<ts.Symbol, string>();
  for (const w of wireTypes) tagBySym.set(w.sym, w.tag);

  const hashLitFor = (sym: ts.Symbol | undefined): string => {
    if (!sym) return 'undefined';
    const tag = tagBySym.get(sym);
    if (!tag) return 'undefined';
    const hex = typeHashes.get(tag);
    if (!hex) return 'undefined';
    return emitHashLiteral(hex);
  };

  const entries: string[] = [];
  for (const s of services) {
    const alias = aliasFor.get(s.sym)!;
    const methodExprs = s.methods.map(m => {
      const reqAlias = m.requestTypeSym && aliasFor.get(m.requestTypeSym);
      const resAlias = m.responseTypeSym && aliasFor.get(m.responseTypeSym);
      const reqFields = deriveMethodFields(wireTypes, m.requestTypeSym);
      const resFields = deriveMethodFields(wireTypes, m.responseTypeSym);
      return `    {
      name: ${JSON.stringify(m.name)},
      pattern: ${patternEnum(m.pattern)},
      requestType: ${reqAlias ?? 'undefined'},
      responseType: ${resAlias ?? 'undefined'},
      acceptsCtx: ${m.acceptsCtx},
      idempotent: ${m.idempotent},
      timeout: ${m.timeout ?? 'undefined'},
      serialization: undefined,
      requires: undefined,
      metadata: undefined,
      requestFields: ${JSON.stringify(reqFields)},
      responseFields: ${JSON.stringify(resFields)},
      requestTypeHash: ${hashLitFor(m.requestTypeSym)},
      responseTypeHash: ${hashLitFor(m.responseTypeSym)},
    },`;
    }).join('\n');
    entries.push(
      `  {
    ctor: ${alias},
    name: ${JSON.stringify(s.name)},
    version: ${s.version},
    scoped: ${JSON.stringify(s.scoped)},
    serializationModes: ${JSON.stringify(s.serializationModes)},
    requires: undefined,
    metadata: undefined,
    methods: [
${methodExprs}
    ],
  },`,
    );
  }
  return `export const SERVICES = [\n${entries.join('\n')}\n] as const;`;
}

function patternEnum(p: ScannedMethod['pattern']): string {
  switch (p) {
    case 'unary': return 'RpcPattern.UNARY';
    case 'server_stream': return 'RpcPattern.SERVER_STREAM';
    case 'client_stream': return 'RpcPattern.CLIENT_STREAM';
    case 'bidi_stream': return 'RpcPattern.BIDI_STREAM';
  }
}

// ─── Manifest field derivation ───────────────────────────────────────────────

/**
 * Convert a scanner-level `ScanField` into the simpler `ManifestField`
 * shape the runtime manifest uses. The manifest type taxonomy is
 * `str|int|float|bool|bytes|list|dict` — it matches what the shell and
 * Mission Control CLI expect today from `runtime.ts:_buildManifest`'s
 * `extractFields` helper. Emitting this at build time replaces the
 * runtime `new Ctor()` + `Object.keys` reflection path, which fails on
 * empty arrays, nullable nested types, and non-default-constructible
 * classes.
 */
interface ManifestFieldOut {
  name: string;
  type: string;
  required: boolean;
  default?: unknown;
}

function scanFieldToManifest(f: ScanField): ManifestFieldOut {
  return {
    name: f.name,
    type: wireToManifestType(f),
    required: !f.nullable,
    default: manifestDefault(f),
  };
}

function wireToManifestType(f: ScanField): string {
  switch (f.kind) {
    case 'primitive':
      switch (f.wire) {
        case 'bool': return 'bool';
        case 'string': return 'str';
        case 'binary': return 'bytes';
        case 'timestamp':
        case 'uuid': return 'str';
        case 'float32':
        case 'float64': return 'float';
        default: return 'int'; // int8/16/32/64, uint8/16/32/64
      }
    case 'ref': return 'dict';
    case 'list':
    case 'set': return 'list';
    case 'map': return 'dict';
  }
}

function manifestDefault(f: ScanField): unknown {
  if (f.nullable) return null;
  switch (f.kind) {
    case 'primitive':
      switch (f.wire) {
        case 'bool': return false;
        case 'string':
        case 'binary':
        case 'timestamp':
        case 'uuid': return '';
        default: return 0;
      }
    case 'ref': return {};
    case 'list':
    case 'set': return [];
    case 'map': return {};
  }
}

function deriveMethodFields(
  wireTypes: ScannedWireType[],
  typeSym: ts.Symbol | undefined,
): ManifestFieldOut[] {
  if (!typeSym) return [];
  const wt = wireTypes.find(w => w.sym === typeSym);
  if (!wt) return [];
  return wt.fields.map(scanFieldToManifest);
}

// ─── Orchestration ───────────────────────────────────────────────────────────

interface LoadedProgram {
  program: ts.Program;
  /** Absolute paths of files listed by the tsconfig's `include`. */
  projectFiles: Set<string>;
}

function loadProgram(projectPath: string): LoadedProgram {
  const configPath = path.resolve(projectPath);
  const { config, error } = ts.readConfigFile(configPath, ts.sys.readFile);
  if (error) {
    throw new ScanError(`failed to read tsconfig: ${ts.flattenDiagnosticMessageText(error.messageText, '\n')}`);
  }
  const parsed = ts.parseJsonConfigFileContent(
    config,
    ts.sys,
    path.dirname(configPath),
  );
  if (parsed.errors.length > 0) {
    const msg = parsed.errors.map(d => ts.flattenDiagnosticMessageText(d.messageText, '\n')).join('\n');
    throw new ScanError(`tsconfig errors: ${msg}`);
  }
  const projectFiles = new Set(parsed.fileNames.map(f => path.resolve(f)));
  return {
    program: ts.createProgram({
      rootNames: parsed.fileNames,
      options: parsed.options,
    }),
    projectFiles,
  };
}

function scan(cli: Cli): {
  wireTypes: ScannedWireType[];
  services: ScannedService[];
  warnings: string[];
} {
  const { program, projectFiles } = loadProgram(cli.project);
  const checker = program.getTypeChecker();
  const ctx: ScanContext = { checker, wireTypeTags: new Map(), warnings: [] };

  const wireTypes: ScannedWireType[] = [];
  const services: ScannedService[] = [];

  const isProjectFile = (sf: ts.SourceFile): boolean => {
    if (sf.isDeclarationFile) return false;
    if (sf.fileName.includes('/node_modules/')) return false;
    // Only scan files explicitly listed in the tsconfig's `include`.
    // Transitively imported files (including other packages in the
    // monorepo) are loaded into the Program so type resolution works
    // but must not contribute their own @WireType / @Service classes.
    return projectFiles.has(path.resolve(sf.fileName));
  };

  // Pass 1a: register every @WireType class's symbol -> tag BEFORE
  // scanning any fields. This lets a class reference itself (e.g.
  // `Entry.children: Entry[]`) — without the pre-pass, `typeToField`
  // would see `Entry` as an unknown symbol because its own tag hadn't
  // been added to `wireTypeTags` yet when its fields were walked.
  const pendingClasses: Array<{ sf: ts.SourceFile; node: ts.ClassDeclaration; decorator: ts.Decorator; tag: string }> = [];
  for (const sf of program.getSourceFiles()) {
    if (!isProjectFile(sf)) continue;
    ts.forEachChild(sf, node => {
      if (!ts.isClassDeclaration(node) || !node.name) return;
      const wt = findDecorator(node, ['WireType']);
      if (!wt) return;
      if (!ts.isCallExpression(wt.expression)) {
        throw new ScanError('@WireType must be called with a tag', locationOf(wt));
      }
      const tagArg = wt.expression.arguments[0];
      if (!tagArg || !ts.isStringLiteralLike(tagArg)) {
        throw new ScanError('@WireType tag must be a string literal', locationOf(wt));
      }
      const sym = ctx.checker.getSymbolAtLocation(node.name!)!;
      ctx.wireTypeTags.set(sym, tagArg.text);
      pendingClasses.push({ sf, node, decorator: wt, tag: tagArg.text });
    });
  }

  // Pass 1b: now that every tag is registered, walk fields and produce
  // the ScannedWireType entries. Self- and mutually-recursive refs now
  // resolve correctly in `typeToField`.
  for (const pc of pendingClasses) {
    const scanned = scanWireType(ctx, pc.node, pc.decorator);
    wireTypes.push(scanned);
    if (cli.verbose) {
      console.error(`  @WireType ${scanned.importName} (${scanned.tag})`);
    }
  }

  // Pass 2: scan @Service classes.
  for (const sf of program.getSourceFiles()) {
    if (!isProjectFile(sf)) continue;
    ts.forEachChild(sf, node => {
      if (!ts.isClassDeclaration(node) || !node.name) return;
      const svc = findDecorator(node, ['Service']);
      if (svc) {
        const scanned = scanService(ctx, node, svc);
        services.push(scanned);
        if (cli.verbose) {
          console.error(`  @Service ${scanned.importName} (${scanned.name} v${scanned.version})`);
        }
      }
    });
  }

  return { wireTypes, services, warnings: ctx.warnings };
}

function emit(
  cli: Cli,
  wireTypes: ScannedWireType[],
  services: ScannedService[],
): string {
  const outPath = path.resolve(cli.out);
  const { hashes: typeHashes, ordered } = computeTypeHashes(wireTypes);
  const { header, aliasFor } = buildImports(outPath, ordered, services);

  return [
    '// @ts-nocheck',
    '// AUTOGENERATED by @aster-rpc/aster — do not edit.',
    '// Regenerate with: npx aster-gen',
    '',
    `import { RpcPattern } from '@aster-rpc/aster';`,
    header,
    '',
    emitWireTypes(ordered, aliasFor),
    '',
    emitServices(ordered, services, aliasFor, typeHashes),
    '',
  ].join('\n');
}

export interface GenerateOptions {
  /** Path to `tsconfig.json`. */
  project: string;
  /** Output path for `aster-rpc.generated.ts`. */
  out: string;
  /** Log discovered classes to stderr. */
  verbose?: boolean;
}

export interface GenerateResult {
  outPath: string;
  wireTypeCount: number;
  serviceCount: number;
  warnings: readonly string[];
}

/**
 * Programmatic entry point: scan a TypeScript project and write
 * `aster-rpc.generated.ts`. Used by the `aster-gen` CLI, the Vite /
 * Webpack plugins, and test harnesses.
 *
 * Throws `ScanError` on unsupported types; callers decide whether to
 * surface as a warning or hard-fail.
 */
export function generate(options: GenerateOptions): GenerateResult {
  const cli: Cli = {
    project: options.project,
    out: options.out,
    verbose: options.verbose ?? false,
  };
  if (cli.verbose) console.error(`aster-gen: scanning ${cli.project}`);
  const { wireTypes, services, warnings } = scan(cli);
  if (cli.verbose) {
    console.error(`aster-gen: found ${wireTypes.length} @WireType, ${services.length} @Service`);
  }
  for (const w of warnings) console.error(`warning: ${w}`);
  const source = emit(cli, wireTypes, services);
  const outPath = path.resolve(cli.out);
  fs.mkdirSync(path.dirname(outPath), { recursive: true });
  fs.writeFileSync(outPath, source, 'utf8');
  return {
    outPath,
    wireTypeCount: wireTypes.length,
    serviceCount: services.length,
    warnings,
  };
}

export { ScanError };

function main(): void {
  const cli = parseArgv(process.argv.slice(2));
  try {
    const result = generate(cli);
    const rel = path.relative(process.cwd(), result.outPath);
    console.error(`aster-gen: wrote ${rel}`);
  } catch (e) {
    if (e instanceof ScanError) {
      console.error(`aster-gen: ${e.message}`);
      process.exit(1);
    }
    throw e;
  }
}

// Run as CLI only when invoked directly as a script. When imported
// by the plugin modules (`import { generate } from '../cli/gen.js'`),
// we must not execute main() as a side effect.
const isDirectCLI = typeof process !== 'undefined' &&
  Array.isArray(process.argv) &&
  typeof process.argv[1] === 'string' &&
  (process.argv[1].endsWith('/cli/gen.js') || process.argv[1].endsWith('\\cli\\gen.js'));
if (isDirectCLI) {
  main();
}

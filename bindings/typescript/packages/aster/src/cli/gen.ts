#!/usr/bin/env node
/**
 * `aster-gen` — build-time scanner for `@aster-rpc/aster` services.
 *
 * Reads a TypeScript project via the TS compiler API, finds every
 * class decorated with `@Service` / `@WireType` from `@aster-rpc/aster`,
 * and emits a `rpc.generated.ts` file that exports `SERVICES` and
 * `WIRE_TYPES` literals for {@link registerGenerated}.
 *
 * See `ffi_spec/ts-buildtime-audit.md` for the design notes and
 * `ffi_spec/Aster-ContractIdentity.md` §11.3.2.3 for the authoritative
 * TS type → wire type mapping.
 *
 * Usage:
 * ```
 *   bunx aster-gen                         # defaults: ./tsconfig.json, ./src/rpc.generated.ts
 *   bunx aster-gen -p tsconfig.app.json
 *   bunx aster-gen -o build/rpc.generated.ts
 * ```
 */

import * as path from 'node:path';
import * as fs from 'node:fs';
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
    out: 'src/rpc.generated.ts',
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
      '  -o, --out <file>           Output file            (default: ./src/rpc.generated.ts)',
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

// ─── Dependency ordering ─────────────────────────────────────────────────────

/**
 * Topological sort of wire types so leaves come first. Naive DFS —
 * does not handle cycles. Cycles in the wire type graph are flagged
 * as a scan error (the NAPI contract_id path needs proper SCC
 * handling, tracked as a follow-up).
 */
function orderWireTypes(
  wireTypes: ScannedWireType[],
): ScannedWireType[] {
  const bySym = new Map<ts.Symbol, ScannedWireType>();
  for (const w of wireTypes) bySym.set(w.sym, w);

  const visited = new Set<ts.Symbol>();
  const inProgress = new Set<ts.Symbol>();
  const ordered: ScannedWireType[] = [];

  function deps(f: ScanField, acc: Set<ts.Symbol>): void {
    if (f.kind === 'ref') {
      for (const [sym, w] of bySym) {
        if (w.tag === f.refTag) acc.add(sym);
      }
    } else if (f.kind === 'list' || f.kind === 'set') {
      deps(f.element, acc);
    } else if (f.kind === 'map') {
      deps(f.key, acc);
      deps(f.value, acc);
    }
  }

  function visit(sym: ts.Symbol): void {
    if (visited.has(sym)) return;
    if (inProgress.has(sym)) {
      const w = bySym.get(sym);
      throw new ScanError(
        `cyclic wire type graph detected (includes ${w?.importName ?? '?'}). ` +
        `Cyclic types are not yet supported by the TS scanner — open an issue.`,
      );
    }
    inProgress.add(sym);
    const w = bySym.get(sym);
    if (!w) {
      inProgress.delete(sym);
      return;
    }
    const ds = new Set<ts.Symbol>();
    for (const f of w.fields) deps(f, ds);
    for (const d of ds) visit(d);
    inProgress.delete(sym);
    visited.add(sym);
    ordered.push(w);
  }

  for (const w of wireTypes) visit(w.sym);
  return ordered;
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
    // foryTypeInfo is intentionally null: the current Fory JS API
    // requires user code to build a typeInfo with a \`buildTypeInfo\`
    // callback (see \`ForyCodec.registerTypeGraph\`). The scanner
    // doesn't know Fory's internal shape. Follow-up: once the Fory
    // JS binding exposes a declarative schema form, the scanner can
    // emit it here and \`registerGenerated\` will feed it to Fory
    // directly — no user callback needed.
    foryTypeInfo: null,
  },`,
    );
  }
  return `export const WIRE_TYPES = [\n${entries.join('\n')}\n] as const;`;
}

function emitServices(
  wireTypes: ScannedWireType[],
  services: ScannedService[],
  aliasFor: Map<ts.Symbol, string>,
): string {
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

  // Pass 1: discover all @WireType classes so services can resolve
  // request/response type refs in pass 2.
  for (const sf of program.getSourceFiles()) {
    if (!isProjectFile(sf)) continue;
    ts.forEachChild(sf, node => {
      if (!ts.isClassDeclaration(node) || !node.name) return;
      const wt = findDecorator(node, ['WireType']);
      if (wt) {
        const scanned = scanWireType(ctx, node, wt);
        wireTypes.push(scanned);
        ctx.wireTypeTags.set(scanned.sym, scanned.tag);
        if (cli.verbose) {
          console.error(`  @WireType ${scanned.importName} (${scanned.tag})`);
        }
      }
    });
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
  const ordered = orderWireTypes(wireTypes);
  const { header, aliasFor } = buildImports(outPath, ordered, services);

  return [
    '// @ts-nocheck',
    '// AUTOGENERATED by @aster-rpc/aster — do not edit.',
    '// Regenerate with: bunx aster-gen',
    '',
    `import { RpcPattern } from '@aster-rpc/aster';`,
    header,
    '',
    emitWireTypes(ordered, aliasFor),
    '',
    emitServices(ordered, services, aliasFor),
    '',
  ].join('\n');
}

export interface GenerateOptions {
  /** Path to `tsconfig.json`. */
  project: string;
  /** Output path for `rpc.generated.ts`. */
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
 * `rpc.generated.ts`. Used by the `aster-gen` CLI, the Vite /
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

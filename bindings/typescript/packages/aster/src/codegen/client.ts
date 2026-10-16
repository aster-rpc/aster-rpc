/**
 * Typed TypeScript clients from Aster contract manifests.
 *
 * Implements docs/_internal/aster-client-generation.md. Lives in this package
 * so generated code tracks this runtime's API; the `aster` CLI runs it as
 * `aster-gen client …` (see `../cli/gen.ts`). Output layout:
 *
 * ```text
 * <out>/<namespace>/
 *   types/<shared-type>.ts, types/<service>-v<version>.ts
 *   services/<service>-v<version>.ts
 *   index.ts
 * ```
 *
 * Ported from the Python CLI's generator; output matched it byte for byte
 * (except the timestamp and float default formatting) until per-call
 * options: every generated method now takes a trailing `callOptions?:
 * CallOptions`, and clients have `withOptions(options)` (Rust's
 * `XClient::with_options`).
 */

import * as fs from 'node:fs';
import * as path from 'node:path';

// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type Manifest = Record<string, any>;
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type Field = Record<string, any>;

interface TypeRecord {
  wireTag: string;
  displayName: string;
  fields: Field[];
  services: Set<string>;
  isRequestResponse: boolean;
}

// ── Naming ───────────────────────────────────────────────────────────────────

/** CamelCase → snake_case (Python `re.sub` rules). */
export function toSnakeCase(name: string): string {
  return name
    .replace(/(.)([A-Z][a-z]+)/g, '$1_$2')
    .replace(/([a-z0-9])([A-Z])/g, '$1_$2')
    .toLowerCase();
}

function toKebabCase(name: string): string {
  return toSnakeCase(name).replace(/_/g, '-');
}

/** Python's code-point string order (`sorted`). */
function byName(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

// ── Type collection (shared with the Python generator) ───────────────────────

/** @aster types referenced by legacy `type` labels without their fields. */
const KNOWN_WIRE_TYPES: Record<string, [string, Field[]]> = {
  DelegationStatement: [
    'aster/DelegationStatement',
    [
      { name: 'authority', type: 'str', required: false, default: 'consumer' },
      { name: 'mode', type: 'str', required: false, default: 'open' },
      { name: 'token_ttl', type: 'int', required: false, default: 300 },
      { name: 'rate_limit', type: 'Optional', required: false, default: null },
      { name: 'roles', type: 'list[str]', required: false, default: null },
    ],
  ],
  SigningKeyAttestation: [
    'aster/SigningKeyAttestation',
    [
      { name: 'signing_pubkey', type: 'str', required: false, default: '' },
      { name: 'key_id', type: 'str', required: false, default: '' },
      { name: 'valid_from', type: 'int', required: false, default: 0 },
      { name: 'valid_until', type: 'int', required: false, default: 0 },
      { name: 'root_signature', type: 'str', required: false, default: '' },
    ],
  ],
};

function collectTypes(manifests: Map<string, Manifest>): Map<string, TypeRecord> {
  const types = new Map<string, TypeRecord>();
  const ensure = (wireTag: string, displayName: string, fields: Field[], service: string) => {
    if (!wireTag && !displayName) return;
    const key = wireTag || displayName;
    let rec = types.get(key);
    if (!rec) {
      rec = { wireTag, displayName, fields, services: new Set(), isRequestResponse: false };
      types.set(key, rec);
    }
    rec.services.add(service);
  };
  const collectElements = (fields: Field[], service: string) => {
    for (const f of fields) {
      const elemTag = f.item_wire_tag || f.element_wire_tag || '';
      const elemName = f.item_ref || f.element_type || '';
      if (elemTag) ensure(elemTag, elemName, f.element_fields ?? [], service);
      if (f.kind === 'ref' && f.wire_tag) ensure(f.wire_tag, f.ref_name ?? '', [], service);
      const known = typeof f.type === 'string' ? KNOWN_WIRE_TYPES[f.type] : undefined;
      if (known) ensure(known[0], f.type, known[1], service);
    }
  };
  for (const [svc, manifest] of manifests) {
    for (const method of manifest.methods ?? []) {
      for (const [tagKey, nameKey, fieldsKey] of [
        ['request_wire_tag', 'request_type', 'fields'],
        ['response_wire_tag', 'response_type', 'response_fields'],
      ] as const) {
        const tag: string = method[tagKey] ?? '';
        const name: string = method[nameKey] ?? '';
        const fields: Field[] = method[fieldsKey] ?? [];
        if (tag || name) {
          ensure(tag, name, fields, svc);
          types.get(tag || name)!.isRequestResponse = true;
          collectElements(fields, svc);
        }
      }
    }
  }
  return types;
}

function classifyTypes(types: Map<string, TypeRecord>): {
  serviceTypes: Map<string, TypeRecord[]>;
  sharedTypes: TypeRecord[];
} {
  const serviceTypes = new Map<string, TypeRecord[]>();
  const sharedTypes: TypeRecord[] = [];
  for (const rec of types.values()) {
    if (rec.services.size === 1) {
      const svc = rec.services.values().next().value as string;
      if (!serviceTypes.has(svc)) serviceTypes.set(svc, []);
      serviceTypes.get(svc)!.push(rec);
    } else {
      sharedTypes.push(rec);
    }
  }
  return { serviceTypes, sharedTypes };
}

// ── Type mapping ─────────────────────────────────────────────────────────────

const TS_PRIMITIVE: Record<string, string> = {
  str: 'string',
  string: 'string',
  int: 'number',
  int32: 'number',
  int64: 'number',
  float: 'number',
  float32: 'number',
  float64: 'number',
  double: 'number',
  bool: 'boolean',
  boolean: 'boolean',
  bytes: 'Uint8Array',
};

const TS_DEFAULT: Record<string, string> = {
  string: '""',
  number: '0',
  boolean: 'false',
  Uint8Array: 'new Uint8Array()',
};

/** Python's `dict.get(key, default)`: an explicit `null` stays `null`. */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
const get = (f: Field, key: string, def: any): any => (key in f ? f[key] : def);

const has = (o: Record<string, string>, k: unknown): k is string =>
  typeof k === 'string' && Object.prototype.hasOwnProperty.call(o, k);

/** `json.dumps(s)`: ASCII-only, non-ASCII as lowercase `\uXXXX`. */
function pyJsonString(s: string): string {
  return JSON.stringify(s).replace(
    /[\u0080-￿]/g,
    (c) => '\\u' + c.charCodeAt(0).toString(16).padStart(4, '0'),
  );
}

function literal(value: unknown): string {
  if (value === null || value === undefined) return 'null as any';
  if (typeof value === 'boolean') return value ? 'true' : 'false';
  if (typeof value === 'number') return String(value);
  if (typeof value === 'string') return pyJsonString(value);
  if (Array.isArray(value)) return '[]';
  if (typeof value === 'object') return '{}';
  return 'undefined as any';
}

function v1Default(f: Field, tsType: string, refDefault: string | null): string {
  switch (f.default_kind) {
    case 'value':
      return literal(f.default_value);
    case 'empty_list':
      return '[]';
    case 'empty_map':
      return '{}';
    case 'null':
      return 'null as any';
  }
  if (refDefault !== null) return refDefault;
  return TS_DEFAULT[tsType] ?? 'undefined as any';
}

function v1Type(f: Field, kind: string, known: Set<string>): [string, string] {
  const ref = (name: string) => (known.has(name) ? name : 'unknown');
  let tsType: string;
  let def: string;
  if (has(TS_PRIMITIVE, kind)) {
    tsType = TS_PRIMITIVE[kind];
    def = v1Default(f, tsType, null);
  } else if (kind === 'list') {
    const itemKind = get(f, 'item_kind', 'string');
    const inner =
      itemKind === 'ref' ? ref(f.item_ref ?? 'unknown') : has(TS_PRIMITIVE, itemKind) ? TS_PRIMITIVE[itemKind] : 'unknown';
    tsType = `${inner}[]`;
    def = '[]';
  } else if (kind === 'map') {
    // JS object keys are always strings.
    const valKind = get(f, 'value_kind', 'string');
    const val =
      valKind === 'ref' ? ref(f.value_ref ?? 'unknown') : has(TS_PRIMITIVE, valKind) ? TS_PRIMITIVE[valKind] : 'unknown';
    tsType = `Record<string, ${val}>`;
    def = '{}';
  } else if (kind === 'ref') {
    tsType = ref(f.ref_name ?? 'unknown');
    def = 'null as any';
  } else if (kind === 'enum') {
    // Aster enums travel as strings.
    tsType = 'string';
    def = '""';
  } else {
    tsType = 'unknown';
    def = 'undefined as any';
  }
  return [tsType, v1Default(f, tsType, def)];
}

function legacyType(f: Field, known: Set<string>): [string, string] {
  const rawOrig = String(f.type ?? 'str');
  const raw = rawOrig.toLowerCase();
  const prim = (k: string) => (has(TS_PRIMITIVE, k) ? TS_PRIMITIVE[k] : known.has(k) ? k : 'unknown');
  if (has(TS_PRIMITIVE, raw)) {
    const ts = TS_PRIMITIVE[raw];
    if (f.default !== null && f.default !== undefined) return [ts, literal(f.default)];
    return [ts, TS_DEFAULT[ts] ?? '""'];
  }
  let m = /^list\[(.+)\]$/.exec(raw);
  if (m) return [`${prim(m[1])}[]`, '[]'];
  if (raw === 'list') return ['unknown[]', '[]'];
  m = /^dict\[(.+),\s*(.+)\]$/.exec(raw);
  if (m) return [`Record<string, ${prim(m[2])}>`, '{}'];
  if (raw === 'dict') return ['Record<string, unknown>', '{}'];
  m = /^optional\[(.+)\]$/.exec(raw);
  if (m) return [`${legacyType({ type: m[1] }, known)[0]} | null`, 'null as any'];
  if (raw === 'optional') return ['unknown | null', 'null as any'];
  if (known.has(rawOrig)) return [rawOrig, 'null as any'];
  return ['unknown', 'undefined as any'];
}

/** (TS type, default expression) for a manifest field, v1 or legacy schema. */
function fieldType(f: Field, known: Set<string>): [string, string] {
  if (f.kind !== undefined && f.kind !== null) {
    let [tsType, def] = v1Type(f, f.kind, known);
    if (f.nullable) {
      tsType = `${tsType} | null`;
      if (f.default_kind === 'none') def = 'null';
    }
    return [tsType, def];
  }
  return legacyType(f, known);
}

// ── Emission ─────────────────────────────────────────────────────────────────

function header(source: string, contractId: string, ts: string): string {
  return (
    '/**\n' +
    ` * Auto-generated by: aster contract client ${source || '<source>'} --lang typescript\n` +
    ` * Contract ID: ${contractId || '<unknown>'}\n` +
    ` * Generated at: ${ts}\n` +
    ` * DO NOT EDIT -- regenerate with: aster contract client ${source || '<source>'} --lang typescript\n` +
    ' */\n'
  );
}

function typeClass(rec: TypeRecord, known: Set<string>): string {
  const n = rec.displayName;
  if (rec.fields.length === 0) {
    return `export class ${n} {\n  constructor(_init?: Partial<${n}>) {}\n}`;
  }
  const lines = [`export class ${n} {`];
  for (const f of rec.fields) {
    const [t, d] = fieldType(f, known);
    lines.push(`  ${f.name}: ${t} = ${d};`);
  }
  lines.push(`  constructor(init?: Partial<${n}>) { if (init) Object.assign(this, init); }`);
  lines.push('}');
  return lines.join('\n');
}

const isSession = (m: Manifest) => m.scoped === 'session' || m.scoped === 'stream';

/** `withOptions` plus the per-call merge every method uses (shared by both client kinds). */
function optionsMembers(cls: string, ctorArgs: string): string[] {
  return [
    '',
    '  /**',
    '   * A copy of this client whose calls (every method, every pattern) carry',
    '   * `options`: deadline, metadata, session id, serialization mode, signal.',
    '   * Replaces options set by an earlier `withOptions`; a call\'s own',
    '   * `callOptions` merge over these (metadata key by key).',
    '   */',
    `  withOptions(options: CallOptions): ${cls} {`,
    `    return new ${cls}(${ctorArgs}, options);`,
    '  }',
    '',
    '  private callOptions(callOptions?: CallOptions): CallOptions {',
    '    const out: CallOptions = { ...this.options };',
    '    for (const [k, v] of Object.entries(callOptions ?? {})) {',
    '      if (v !== undefined) (out as Record<string, unknown>)[k] = v;',
    '    }',
    '    out.metadata = { ...this.options?.metadata, ...callOptions?.metadata };',
    `    out.version = ${cls}.serviceVersion;`,
    '    return out;',
    '  }',
  ];
}

function fromConnectionPreamble(svc: string, cls: string): string[] {
  return [
    `  static async fromConnection(client: AsterClientWrapper): Promise<${cls}> {`,
    `    const summary = (await client.listServices()).find((s: { name: string }) => s.name === "${svc}");`,
    '    if (!summary) {',
    `      throw new Error("${svc} not offered by this peer (did you connect to the right address?)");`,
    '    }',
  ];
}

function sharedClient(svc: string, version: unknown, cid: string, methods: Manifest[], known: Set<string>, cls: string): string {
  const lines = [
    `/** Typed client for ${svc} v${version}. */`,
    `export class ${cls} {`,
    `  static readonly serviceName = "${svc}";`,
    `  static readonly serviceVersion = ${version};`,
    `  static readonly contractId = "${cid}";`,
    '',
    '  private constructor(',
    '    private readonly transport: AsterTransport,',
    '    private readonly options?: CallOptions,',
    '  ) {}',
    '',
    '  /**',
    `   * Resolve ${svc} on a connected AsterClient and return a`,
    '   * typed client wired to the existing RPC transport.',
    '   */',
    ...fromConnectionPreamble(svc, cls),
    "    // Reuse the wrapper's already-connected RPC transport.",
    '    const transport: AsterTransport | undefined =',
    '      (client as unknown as { transport?: AsterTransport }).transport;',
    '    if (!transport) {',
    '      throw new Error("AsterClient has no transport; call await client.connect() first");',
    '    }',
    `    return new ${cls}(transport);`,
    '  }',
    '',
    '  /** Close the underlying transport. */',
    '  async close(): Promise<void> {',
    '    await this.transport.close();',
    '  }',
    ...optionsMembers(cls, 'this.transport'),
  ];
  for (const m of methods) {
    const name: string = m.name;
    const pattern: string = m.pattern ?? 'unary';
    const req = known.has(m.request_type ?? '') ? m.request_type : 'unknown';
    const resp = known.has(m.response_type ?? '') ? m.response_type : 'unknown';
    const inline = m.request_style === 'inline';
    const params: Field[] = m.inline_params || [];
    const sig = params.map((p) => `${p.name ?? 'arg'}: ${fieldType(p, known)[0]}`).join(', ');
    const optArg = 'callOptions?: CallOptions';
    const withOpt = (a: string) => (a ? `${a}, ${optArg}` : optArg);
    const opts = 'this.callOptions(callOptions)';
    const ctor = params.length
      ? `new ${req}({${params.map((p) => p.name ?? 'arg').join(', ')}})`
      : `new ${req}()`;
    lines.push('');
    if (pattern === 'unary') {
      const [args, arg] = inline ? [sig, ctor] : [`request: ${req}`, 'request'];
      lines.push(`  async ${name}(${withOpt(args)}): Promise<${resp}> {`);
      lines.push(`    return (await this.transport.unary("${svc}", "${name}", ${arg}, ${opts})) as ${resp};`);
      lines.push('  }');
    } else if (pattern === 'server_stream') {
      const [args, arg] = inline ? [sig, ctor] : [`request: ${req}`, 'request'];
      lines.push(`  async *${name}(${withOpt(args)}): AsyncGenerator<${resp}> {`);
      lines.push(`    for await (const item of this.transport.serverStream("${svc}", "${name}", ${arg}, ${opts})) {`);
      lines.push(`      yield item as ${resp};`);
      lines.push('    }');
      lines.push('  }');
    } else if (pattern === 'client_stream') {
      lines.push(`  async ${name}(requests: AsyncIterable<${req}>, ${optArg}): Promise<${resp}> {`);
      lines.push(`    return (await this.transport.clientStream("${svc}", "${name}", requests, ${opts})) as ${resp};`);
      lines.push('  }');
    } else if (pattern === 'bidi_stream') {
      lines.push(`  ${name}(${optArg}): BidiChannel {`);
      lines.push(`    return this.transport.bidiStream("${svc}", "${name}", ${opts});`);
      lines.push('  }');
    } else {
      lines.push(`  // unknown pattern: ${pattern}`);
    }
  }
  lines.push('}');
  return lines.join('\n');
}

function sessionClient(svc: string, version: unknown, cid: string, methods: Manifest[], known: Set<string>, cls: string): string {
  const lines = [
    `/** Typed client for session-scoped ${svc} v${version}. */`,
    `export class ${cls} {`,
    `  static readonly serviceName = "${svc}";`,
    `  static readonly serviceVersion = ${version};`,
    `  static readonly contractId = "${cid}";`,
    '',
    '  // SessionProxyClient is dynamically typed (JS Proxy with method-name',
    '  // interception); we wrap it with typed signatures below.',
    '  // eslint-disable-next-line @typescript-eslint/no-explicit-any',
    '  private constructor(',
    '    private readonly inner: any,',
    '    private readonly options?: CallOptions,',
    '  ) {}',
    '',
    '  /**',
    `   * Open a session-scoped ${svc} client. The first method`,
    '   * call opens the underlying bidi stream; subsequent calls reuse',
    `   * it so per-${svc} state survives across calls.`,
    '   */',
    ...fromConnectionPreamble(svc, cls),
    `    const inner = await client.proxy("${svc}");`,
    `    return new ${cls}(inner);`,
    '  }',
    '',
    '  /** Close this session, releasing the underlying bidi stream. */',
    '  async close(): Promise<void> {',
    '    await this.inner.close();',
    '  }',
    ...optionsMembers(cls, 'this.inner'),
  ];
  for (const m of methods) {
    const name: string = m.name;
    const pattern: string = m.pattern ?? 'unary';
    const req = known.has(m.request_type ?? '') ? m.request_type : 'unknown';
    const resp = known.has(m.response_type ?? '') ? m.response_type : 'unknown';
    lines.push('');
    if (pattern === 'unary') {
      lines.push(`  async ${name}(request: ${req}, callOptions?: CallOptions): Promise<${resp}> {`);
      lines.push(`    return (await this.inner.${name}(request, this.callOptions(callOptions))) as ${resp};`);
      lines.push('  }');
    } else {
      lines.push(`  // session-scoped ${pattern} is not yet supported by SessionProxyClient`);
      lines.push(`  async ${name}(_request: ${req}, _callOptions?: CallOptions): Promise<${resp}> {`);
      lines.push(
        `    throw new Error("${svc}.${name}: session-scoped ${pattern} methods are not yet supported by the TypeScript runtime");`,
      );
      lines.push('  }');
    }
  }
  lines.push('}');
  return lines.join('\n');
}

function serviceClient(svc: string, manifest: Manifest, known: Set<string>): string {
  const methods = [...(manifest.methods ?? [])].sort((a, b) => byName(a.name ?? '', b.name ?? ''));
  const version = manifest.version ?? 1;
  const cid = manifest.contract_id ?? '';
  const emit = isSession(manifest) ? sessionClient : sharedClient;
  return emit(svc, version, cid, methods, known, `${svc}Client`);
}

/** Every plausible class reference in a manifest field. */
function candidateRefs(f: Field): string[] {
  const out: string[] = [];
  if (f.kind === 'ref') out.push(f.ref_name ?? '');
  if (f.item_kind === 'ref') out.push(f.item_ref ?? '');
  if (f.value_kind === 'ref') out.push(f.value_ref ?? '');
  out.push(f.element_type ?? '');
  if (typeof f.type === 'string') {
    const m = /^list\[(.+)\]$/.exec(f.type);
    if (m) out.push(m[1]);
    else if (f.type && /^[A-Z]/.test(f.type)) out.push(f.type);
  }
  return out.filter((c) => c);
}

// ── Entry point ──────────────────────────────────────────────────────────────

export interface GenerateOptions {
  outDir: string;
  namespace: string;
  /** Recorded in file headers. */
  source?: string;
  /** Header timestamp; defaults to now (UTC, seconds). */
  generatedAt?: string;
}

/** Write client files for `manifests` (service name → manifest); returns their paths. */
export function generateTypescriptClients(manifestList: Manifest[], opts: GenerateOptions): string[] {
  const manifests = new Map<string, Manifest>();
  for (const item of manifestList) {
    const m = item.contract ?? item; // .aster.json exports wrap the manifest
    manifests.set(m.service ?? 'UnknownService', m);
  }
  const source = opts.source ?? '';
  const ts = opts.generatedAt ?? new Date().toISOString().replace(/\.\d{3}Z$/, 'Z');
  const generated: string[] = [];
  const write = (p: string, content: string) => {
    fs.mkdirSync(path.dirname(p), { recursive: true });
    fs.writeFileSync(p, content);
    generated.push(p);
  };

  const allTypes = collectTypes(manifests);
  const { serviceTypes, sharedTypes } = classifyTypes(allTypes);
  const known = new Set([...allTypes.values()].map((r) => r.displayName));
  const sharedNames = new Set(sharedTypes.map((r) => r.displayName));
  const nsDir = path.join(opts.outDir, toSnakeCase(opts.namespace));
  const typesDir = path.join(nsDir, 'types');
  const servicesDir = path.join(nsDir, 'services');
  fs.mkdirSync(typesDir, { recursive: true });
  fs.mkdirSync(servicesDir, { recursive: true });

  const firstCid: string = manifests.values().next().value?.contract_id ?? '';
  const sortedShared = [...sharedTypes].sort((a, b) => byName(a.displayName, b.displayName));
  const sortedServices = [...manifests.entries()].sort((a, b) => byName(a[0], b[0]));
  const stem = (svc: string, m: Manifest) => `${toKebabCase(svc)}-v${m.version ?? 1}`;

  for (const rec of sortedShared) {
    write(
      path.join(typesDir, toKebabCase(rec.displayName) + '.ts'),
      header(source, firstCid, ts) + '\n' + typeClass(rec, known) + '\n',
    );
  }

  for (const [svc, m] of sortedServices) {
    const recs = serviceTypes.get(svc) ?? [];
    if (recs.length === 0) continue;
    let body = header(source, m.contract_id ?? firstCid, ts) + '\n';
    const seen = new Set<string>();
    const imports: string[] = [];
    for (const rec of recs) {
      for (const f of rec.fields) {
        for (const c of candidateRefs(f)) {
          if (sharedNames.has(c) && !seen.has(c)) {
            seen.add(c);
            imports.push(c);
          }
        }
      }
    }
    imports.sort(byName);
    for (const name of imports) body += `import { ${name} } from "./${toKebabCase(name)}.js";\n`;
    if (imports.length) body += '\n';
    for (const rec of [...recs].sort((a, b) => byName(a.displayName, b.displayName))) {
      body += typeClass(rec, known) + '\n\n';
    }
    write(path.join(typesDir, stem(svc, m) + '.ts'), body.trimEnd() + '\n');
  }

  for (const [svc, m] of sortedServices) {
    let body = header(source, m.contract_id ?? firstCid, ts) + '\n';
    const parts = ['AsterClientWrapper'];
    if (!isSession(m)) {
      parts.push('type AsterTransport');
      if ((m.methods ?? []).some((x: Manifest) => (x.pattern ?? 'unary') === 'bidi_stream')) {
        parts.push('type BidiChannel');
      }
    }
    parts.push('type CallOptions');
    body += `import { ${parts.join(', ')} } from "@aster-rpc/aster";\n`;

    const byFile = new Map<string, Set<string>>();
    const seen = new Set<string>();
    const add = (name: string) => {
      if (!name || seen.has(name) || ['None', 'Any', 'unknown'].includes(name) || !known.has(name)) return;
      seen.add(name);
      const file = sharedNames.has(name) ? toKebabCase(name) : stem(svc, m);
      if (!byFile.has(file)) byFile.set(file, new Set());
      byFile.get(file)!.add(name);
    };
    for (const method of m.methods ?? []) {
      add(method.request_type ?? '');
      add(method.response_type ?? '');
      for (const key of ['fields', 'response_fields']) {
        for (const f of method[key] ?? []) for (const c of candidateRefs(f)) add(c);
      }
    }
    for (const file of [...byFile.keys()].sort(byName)) {
      body += `import { ${[...byFile.get(file)!].sort(byName).join(', ')} } from "../types/${file}.js";\n`;
    }
    body += '\n' + serviceClient(svc, m, known) + '\n';
    write(path.join(servicesDir, stem(svc, m) + '.ts'), body);
  }

  const index = [header(source, firstCid, ts), ''];
  for (const [svc, m] of sortedServices) index.push(`export * from "./services/${stem(svc, m)}.js";`);
  for (const rec of sortedShared) index.push(`export * from "./types/${toKebabCase(rec.displayName)}.js";`);
  for (const [svc, m] of sortedServices) {
    if (serviceTypes.get(svc)?.length) index.push(`export * from "./types/${stem(svc, m)}.js";`);
  }
  write(path.join(nsDir, 'index.ts'), index.join('\n') + '\n');
  return generated;
}

/** The snippet printed after generation. */
export function formatUsageSnippet(outDir: string, namespace: string, manifestList: Manifest[], address = ''): string {
  const ns = toSnakeCase(namespace);
  const first = manifestList[0]?.contract ?? manifestList[0] ?? {};
  const svc: string = first.service ?? 'MyService';
  const svcStem = `${toKebabCase(svc)}-v${first.version ?? 1}`;
  const lines = [
    `\nGenerated TypeScript clients -> ${outDir}/${ns}/\n`,
    'Usage:\n',
    '  import { AsterClientWrapper } from "@aster-rpc/aster";',
    `  import { ${svc}Client } from "./${ns}/services/${svcStem}.js";`,
  ];
  const example = (first.methods ?? []).find((m: Manifest) => (m.pattern ?? 'unary') === 'unary');
  const req: string = example?.request_type ?? '';
  if (req) {
    // Shared types (used by several services) get a file of their own.
    const all = new Map<string, Manifest>(manifestList.map((m) => [(m.contract ?? m).service, m.contract ?? m]));
    const shared = classifyTypes(collectTypes(all)).sharedTypes.some((r) => r.displayName === req);
    lines.push(`  import { ${req} } from "./${ns}/types/${shared ? toKebabCase(req) : svcStem}.js";`);
  }
  lines.push('');
  lines.push(`  const client = new AsterClientWrapper({ address: "${address || 'aster1...'}" });`);
  lines.push('  await client.connect();');
  lines.push(`  const svc = await ${svc}Client.fromConnection(client);`);
  if (example && req) {
    lines.push(`  const result = await svc.${example.name}(new ${req}({}));`);
    lines.push('  console.log(result);');
  }
  return lines.join('\n');
}

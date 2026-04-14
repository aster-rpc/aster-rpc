# @aster-rpc/aster

Aster RPC framework for TypeScript — peer-to-peer services with type
safety, streaming, and trust, built on [Iroh](https://github.com/n0-computer/iroh)
(QUIC, blobs, docs, gossip).

Runs on Node.js 20+, Bun 1.0+, and Deno via Node compat.

---

## Install

```sh
npm install @aster-rpc/aster
# or
bun add @aster-rpc/aster
```

## Defining a service

Write a class with `@Service`, wire types with `@WireType`, and
methods with `@Rpc` / `@ServerStream` / `@ClientStream` / `@BidiStream`.
The method's parameter and return types *are* the schema — no
explicit `{ request, response }` options, no decorator metadata
boilerplate.

```ts
import {
  Service, Rpc, ServerStream, WireType,
  type i64,
} from '@aster-rpc/aster';

@WireType('mission/StatusRequest')
export class StatusRequest {
  agentId = '';
  nonce: i64 = 0n as i64;
}

@WireType('mission/StatusResponse')
export class StatusResponse {
  status = '';
  uptime: i64 = 0n as i64;
  warnings: string[] = [];
}

@WireType('mission/StatusEvent')
export class StatusEvent {
  at = new Date();
  status = new StatusResponse();
  note?: string;
}

@Service({ name: 'MissionControl', version: 1 })
export class MissionControlService {
  @Rpc({ timeout: 30, idempotent: true })
  async getStatus(req: StatusRequest): Promise<StatusResponse> {
    const res = new StatusResponse();
    res.status = 'running';
    return res;
  }

  @ServerStream()
  async *watchStatus(req: StatusRequest): AsyncGenerator<StatusEvent> {
    yield new StatusEvent();
  }
}
```

## Wire types are TypeScript types

Field types map to wire types per spec §11.3.2.3:

| TS type | Wire type | Notes |
|---|---|---|
| `number` | `float64` | `number` is IEEE 754 double — honest mapping |
| `bigint` | `int64` | Use for integers on the wire |
| `boolean` | `bool` | |
| `string` | `string` | |
| `Uint8Array` | `binary` | |
| `Date` | `timestamp` | |
| `T[]`, `Array<T>`, `ReadonlyArray<T>` | `list<T>` | |
| `Map<K,V>` | `map<K,V>` | |
| `Set<T>` | `set<T>` | |
| `T \| null`, `T \| undefined`, `field?:` | `nullable<T>` | All three forms collapse to the same wire type |
| class tagged `@WireType('foo/Bar')` | ref | Transitive wire types auto-discovered |

**For narrower integers**, import the branded aliases and annotate
the field:

```ts
import type { i32, u32, f32 } from '@aster-rpc/aster';

@WireType('billing/Quantity')
export class Quantity {
  count: i32 = 0 as i32;
  scale: f32 = 1.0 as f32;
}
```

Available brands: `i8`, `i16`, `i32`, `i64`, `u8`, `u16`, `u32`,
`u64`, `f32`, `f64`. `i64` / `u64` wrap `bigint`; the rest wrap
`number`.

**Suspicious-field warning.** A plain `number` field whose name
matches `count|id|size|length|index|offset|timestamp|epoch|...`
emits a build-time warning suggesting `bigint` or a brand. Suppress
by fixing the type or explicitly annotating with `f64`.

## Running the scanner

Wire types are erased at runtime, so `@aster-rpc/aster` ships a
build-time scanner — `aster-gen` — that walks your `tsconfig.json`,
reads field and parameter types from the TypeScript compiler API,
and emits a `rpc.generated.ts` file with ready-made service +
wire-type metadata.

```sh
bunx aster-gen
# or
npx aster-gen
```

Defaults: reads `./tsconfig.json`, writes `./src/rpc.generated.ts`.
Override with `-p` / `--project` and `-o` / `--out`:

```sh
bunx aster-gen -p tsconfig.app.json -o build/rpc.generated.ts
```

Add it to `package.json` so builds and CI always regenerate:

```json
{
  "scripts": {
    "gen": "aster-gen",
    "build": "aster-gen && tsc"
  }
}
```

### Vite

```ts
// vite.config.ts
import { defineConfig } from 'vite';
import { asterGen } from '@aster-rpc/aster/vite-plugin';

export default defineConfig({
  plugins: [
    asterGen({
      project: 'tsconfig.json',
      out: 'src/rpc.generated.ts',
    }),
  ],
});
```

The plugin regenerates on every `buildStart` and on HMR when a `.ts`
file changes.

### Webpack

```js
// webpack.config.js
const { AsterGenWebpackPlugin } = require('@aster-rpc/aster/webpack-plugin');

module.exports = {
  plugins: [
    new AsterGenWebpackPlugin({
      project: 'tsconfig.json',
      out: 'src/rpc.generated.ts',
    }),
  ],
};
```

### Wiring the generated file

Once per process, before constructing `AsterServer`, call
`registerGenerated` with the exports from your `rpc.generated.ts`:

```ts
import { AsterServer, registerGenerated } from '@aster-rpc/aster';
import { SERVICES, WIRE_TYPES } from './rpc.generated.js';
import { MissionControlService } from './services/mission_control.js';

registerGenerated({ SERVICES, WIRE_TYPES });

const server = new AsterServer({
  services: [new MissionControlService()],
});
await server.start();
```

`registerGenerated` stamps the pre-built metadata onto each class
constructor, so the existing runtime paths (`ServiceRegistry`,
manifest publication, JSON shape validation) consume the generated
data instead of reflecting at runtime.

## What happens if you don't run `aster-gen`?

Everything still works. The runtime falls back to reflection
(`new cls()` + `Object.keys`, plus `Function.length` for CallContext
detection) and logs a warning once per class / service / method:

```
[aster] StatusRequest: falling back to runtime introspection (new cls() + Object.keys).
  Run 'bunx aster-gen' and import the generated file once at startup...
```

Reflection has documented limitations:

- Empty array fields don't reveal their element type
- Optional / `null` nested types don't get recursed into
- Classes that need constructor arguments can't be introspected

Running `aster-gen` fixes all three — it reads types from the AST,
not from runtime values.

## Brand types vs `@Rpc({ request, response })`

Earlier versions of `@aster-rpc/aster` required
`@Rpc({ request: Req, response: Res })` so the runtime could reach
the constructors despite type erasure. With `aster-gen` the
decorator options are redundant — the scanner reads the types from
the AST directly. You can delete them from new code. They still
work (and still trigger the runtime manifest path) if you haven't
run the scanner yet.

## API reference

Generated API docs (TypeDoc): `bun run docs` produces `docs-api/`.

## License

Apache-2.0.

#!/usr/bin/env python3
"""Run clean Python and TypeScript consumers against exact local candidate artifacts.

This orchestrator deliberately runs only from fresh temporary projects outside
the checkout. It installs the supplied wheel/npm tarballs; it does not build
them. See --help for required candidate artifact/hash arguments.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import zipfile


PYTHON_APP = r'''import asyncio, hashlib, importlib.metadata as md, json, os, sys, time
from pathlib import Path
from dataclasses import dataclass
from typing import AsyncIterator

import aster
import aster._aster as native
from aster import AsterClient, AsterConfig, AsterServer, RpcError, StatusCode, rpc, server_stream, service, wire_type

root = Path.cwd().resolve()
for label, mod in (("aster", aster), ("native", native)):
    p = Path(mod.__file__).resolve()
    if root not in p.parents:
        raise AssertionError(f"{label} import escaped clean consumer: {p}")
expected_version = os.environ["ASTER_SMOKE_VERSION"]
native_hash = hashlib.sha256(Path(native.__file__).read_bytes()).hexdigest()
assert native_hash == os.environ["ASTER_SMOKE_PY_NATIVE_SHA256"], "installed native module differs from candidate wheel"
if aster.VERSION != expected_version:
    raise AssertionError(f"unexpected aster.VERSION={aster.VERSION!r}; expected {expected_version!r}")
distribution_version = md.version("aster-rpc")
if distribution_version != expected_version:
    raise AssertionError(f"unexpected aster-rpc distribution version {distribution_version!r}")
print(json.dumps({"phase":"python_metadata", "version":aster.VERSION,
    "distribution":distribution_version, "module":str(Path(aster.__file__).resolve()),
    "native":str(Path(native.__file__).resolve()), "native_sha256":native_hash, "python":sys.version}, sort_keys=True), flush=True)

@wire_type("assurance.clean/Request")
@dataclass
class Request:
    value: str = ""
    count: int = 0

@wire_type("assurance.clean/Reply")
@dataclass
class Reply:
    value: str = ""
    count: int = 0

cancel_seen = asyncio.Event()
deadline_seen = asyncio.Event()
@service(name="AssuranceCleanConsumer", version=1)
class Smoke:
    @rpc(timeout=0.25)
    async def delayed(self, req: Request) -> Reply:
        try:
            await asyncio.sleep(5)
        except asyncio.CancelledError:
            deadline_seen.set()
            raise
        return Reply(value=req.value)

    @rpc(timeout=20)
    async def echo(self, req: Request) -> Reply:
        return Reply(value="echo:" + req.value)

    @server_stream
    async def sequence(self, req: Request) -> AsyncIterator[Reply]:
        for i in range(req.count):
            yield Reply(value=req.value, count=i)

    @rpc(timeout=20)
    async def hold(self, req: Request) -> Reply:
        try:
            await asyncio.sleep(60)
        except asyncio.CancelledError:
            cancel_seen.set()
            raise
        return Reply(value=req.value)

async def main():
    cfg = AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0")
    counts = {"unary":0, "stream":0, "deadline":0, "cancel":0, "shutdown":0}
    async with AsterServer(services=[Smoke()], allow_all_consumers=True, config=cfg) as server:
        async with AsterClient(endpoint_addr=server.endpoint_addr_b64, config=cfg) as client:
            svc = await client.client(Smoke)
            out = await asyncio.wait_for(svc.echo(Request(value="candidate")), 12)
            assert out.value == "echo:candidate"; counts["unary"] += 1
            rows = [r async for r in svc.sequence(Request(value="s", count=5))]
            assert [r.count for r in rows] == list(range(5)); counts["stream"] = len(rows)
            # Method timeout must become a deadline failure and cancel its handler.
            started = time.monotonic()
            try:
                await asyncio.wait_for(svc.delayed(Request(value="deadline")), 4)
            except RpcError as exc:
                assert exc.code == StatusCode.DEADLINE_EXCEEDED, f"wrong deadline status: {exc.code}"
            else:
                raise AssertionError("configured RPC deadline did not fail")
            elapsed = time.monotonic() - started
            assert 0.08 <= elapsed < 2.0, f"deadline took {elapsed:.3f}s, expected near 0.25s"
            await asyncio.wait_for(deadline_seen.wait(), 4); counts["deadline"] = 1
            task = asyncio.create_task(svc.hold(Request(value="cancel")))
            await asyncio.sleep(0.2); task.cancel()
            try: await task
            except asyncio.CancelledError: pass
            else: raise AssertionError("caller cancellation did not cancel RPC")
            await asyncio.wait_for(cancel_seen.wait(), 4); counts["cancel"] = 1
            assert counts == {"unary":1,"stream":5,"deadline":1,"cancel":1,"shutdown":0}
    counts["shutdown"] = 1 # both public async context managers exited cleanly
    assert counts["shutdown"] == 1
    print(json.dumps({"phase":"python_result", "result":"PASS", "counts":counts}, sort_keys=True), flush=True)

asyncio.run(main())
'''


TS_APP = r'''import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { AsterServer, AsterClientWrapper, CallContext, Rpc, RpcError, ServerStream, Service, StatusCode, WireType } from '@aster-rpc/aster';

@WireType('assurance.clean/Request') class Request { value = ''; count = 0; }
@WireType('assurance.clean/Reply') class Reply { value = ''; count = 0; }
let cancelSeen = false, deadlineSeen = false;
@Service({name:'AssuranceCleanConsumerTS',version:1})
class Smoke {
  @Rpc({request:Request,response:Reply,timeout:20})
  async echo(req:Request):Promise<Reply> { const r=new Reply(); r.value='echo:'+req.value; return r; }
  @ServerStream({request:Request,response:Reply})
  async *sequence(req:Request):AsyncGenerator<Reply> { for(let i=0;i<req.count;i++){const r=new Reply();r.value=req.value;r.count=i;yield r;} }
  @Rpc({request:Request,response:Reply,timeout:20})
  async hold(req:Request, ctx:CallContext):Promise<Reply> {
    await new Promise<void>((resolve,reject)=>{const t=setTimeout(resolve,60000);ctx.signal.addEventListener('abort',()=>{clearTimeout(t);cancelSeen=true;reject(ctx.signal.reason);},{once:true});});
    const r=new Reply();r.value=req.value;return r;
  }
  @Rpc({request:Request,response:Reply,timeout:20})
  async delayed(req:Request, ctx:CallContext):Promise<Reply> {
    await new Promise<void>((resolve,reject)=>{const t=setTimeout(resolve,5000);ctx.signal.addEventListener('abort',()=>{clearTimeout(t);deadlineSeen=true;reject(ctx.signal.reason);},{once:true});});
    const r=new Reply();r.value=req.value;return r;
  }
}

const root=process.env.ASTER_CONSUMER_ROOT ?? process.cwd();
const require=createRequire(import.meta.url);
const asterEntry=fileURLToPath(import.meta.resolve('@aster-rpc/aster'));
const loader=require.resolve('@aster-rpc/transport');
const sidecar=require.resolve('@aster-rpc/transport-linux-x64-gnu');
const pkg=(p:string)=>JSON.parse(readFileSync(p,'utf8'));
const versions={aster:pkg(join(dirname(asterEntry),'..','package.json')).version,
  transport:pkg(join(dirname(loader),'package.json')).version,sidecar:pkg(join(dirname(sidecar),'package.json')).version};
for(const [k,p] of Object.entries({aster:asterEntry,transport:loader,sidecar})) if(!p.startsWith(root+'/')) throw Error(`${k} resolved outside clean project: ${p}`);
const expectedVersion=process.env.ASTER_SMOKE_VERSION;
if(Object.values(versions).some(v=>v!==expectedVersion)) throw Error(`unexpected package versions ${JSON.stringify(versions)}; expected ${expectedVersion}`);
console.log(JSON.stringify({phase:'typescript_metadata',node:process.version,versions,asterEntry,loader,sidecar}));

const server=new AsterServer({services:[new Smoke()],allowAllConsumers:true,config:{relayMode:'disabled',bindAddr:'127.0.0.1:0'}});
let client:AsterClientWrapper|undefined;
let serveTask:Promise<void>|undefined;
try {
  await server.start();
  serveTask=server.serve();
  client=new AsterClientWrapper({address:server.address,config:{relayMode:'disabled',bindAddr:'127.0.0.1:0'}});
  await client.connect(); const svc=await client.client(Smoke);
  const result=await svc.echo(Object.assign(new Request(),{value:'candidate'}));
  if(result.value!=='echo:candidate') throw Error('unary mismatch');
  const rows:Reply[]=[]; for await(const row of svc.sequence(Object.assign(new Request(),{value:'s',count:5}))) rows.push(row);
  if(rows.length!==5||rows.some((r,i)=>r.count!==i)) throw Error('stream mismatch');
  const ac=new AbortController(); const hold=(svc as any).hold(Object.assign(new Request(),{value:'cancel'}),{signal:ac.signal});
  await new Promise(r=>setTimeout(r,150)); ac.abort();
  try { await hold; throw Error('aborted call unexpectedly succeeded'); }
  catch(e) { if(e instanceof Error && e.message==='aborted call unexpectedly succeeded') throw e;
    if(!(e instanceof RpcError) || e.code!==StatusCode.CANCELLED) throw Error(`wrong cancel result: ${String(e)}`); }
  await waitUntil(()=>cancelSeen,4000,'server handler cancel acknowledgement');
  const deadlineStarted=Date.now();
  try { await (svc as any).delayed(Object.assign(new Request(),{value:'deadline'}),{deadlineSecs:0.2}); throw Error('deadline call unexpectedly succeeded'); }
  catch(e) { if(e instanceof Error && e.message==='deadline call unexpectedly succeeded') throw e;
    if(!(e instanceof RpcError) || e.code!==StatusCode.DEADLINE_EXCEEDED) throw Error(`wrong deadline result: ${String(e)}`); }
  const deadlineElapsed=Date.now()-deadlineStarted;
  if(deadlineElapsed<80||deadlineElapsed>=2000) throw Error(`deadline took ${deadlineElapsed}ms`);
  await waitUntil(()=>deadlineSeen,4000,'server handler deadline acknowledgement');
  await client.close(); client=undefined; await server.close(); await serveTask;
  console.log(JSON.stringify({phase:'typescript_result',result:'PASS',counts:{unary:1,stream:rows.length,cancel:1,deadline:1,shutdown:1}}));
} finally { await client?.close(); await server.close(); if(serveTask) await serveTask; }

async function waitUntil(check:()=>boolean, timeoutMs:number, what:string):Promise<void> {
  const end=Date.now()+timeoutMs;
  while(!check()) { if(Date.now()>=end) throw Error(`timed out waiting for ${what}`); await new Promise(r=>setTimeout(r,10)); }
}
'''


def run(cmd: list[str], *, cwd: Path, env: dict[str, str], log_path: Path) -> None:
    with log_path.open("w", encoding="utf-8") as log:
        print("$", " ".join(cmd), flush=True)
        p = subprocess.run(cmd, cwd=cwd, env=env, stdout=log, stderr=subprocess.STDOUT)
    print(log_path.read_text(errors="replace")[-12000:], end="", flush=True)
    if p.returncode:
        raise SystemExit(f"command failed ({p.returncode}); full output: {log_path}")


def sha(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def arg_artifact(parser: argparse.ArgumentParser, name: str, help_text: str) -> None:
    parser.add_argument(f"--{name}", type=Path, help=help_text)
    parser.add_argument(f"--{name}-sha256", help="Expected lowercase SHA-256")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--checkout-root", type=Path, help="source checkout to exclude; required when this harness is copied outside the checkout")
    ap.add_argument("--python", action="store_true", help="Run isolated Python wheel consumer")
    ap.add_argument("--typescript", action="store_true", help="Run isolated npm tarball consumer")
    ap.add_argument("--version", default="0.3.705", help="Exact expected package version (default: 0.3.705)")
    arg_artifact(ap, "wheel", "candidate aster-rpc 0.3.705 manylinux x86_64 wheel")
    arg_artifact(ap, "npm-aster", "candidate @aster-rpc/aster tarball")
    arg_artifact(ap, "npm-transport", "candidate @aster-rpc/transport loader tarball")
    arg_artifact(ap, "npm-linux-x64-gnu", "candidate @aster-rpc/transport-linux-x64-gnu sidecar tarball")
    a = ap.parse_args()
    if not a.python and not a.typescript:
        ap.error("select at least one of --python or --typescript")
    artifacts = {
        "wheel": (a.wheel, a.wheel_sha256),
        "npm-aster": (a.npm_aster, a.npm_aster_sha256),
        "npm-transport": (a.npm_transport, a.npm_transport_sha256),
        "npm-linux-x64-gnu": (a.npm_linux_x64_gnu, a.npm_linux_x64_gnu_sha256),
    }
    selected = ["wheel"] if a.python else []
    if a.typescript:
        selected += ["npm-aster", "npm-transport", "npm-linux-x64-gnu"]
    for name in selected:
        path, expected = artifacts[name]
        if path is None or expected is None:
            ap.error(f"selected artifact {name} requires both --{name} and --{name}-sha256")
        path = path.resolve(strict=True)
        actual = sha(path)
        if actual.lower() != expected.lower():
            raise SystemExit(f"SHA-256 mismatch for {name}: {actual} != {expected}")
        artifacts[name] = (path, actual)

    if a.checkout_root is not None:
        repo = a.checkout_root.resolve(strict=True)
    else:
        repo = next((p for p in Path(__file__).resolve().parents if (p / ".git").exists()), None)
        if repo is None:
            ap.error("relocated harness requires --checkout-root")
    work = Path(tempfile.mkdtemp(prefix="aster-clean-consumer-", dir=os.environ.get("TMPDIR", "/tmp"))).resolve()
    if work == repo or repo in work.parents or work in repo.parents:
        raise SystemExit(f"temporary consumer must be isolated from checkout: {work}")
    print(json.dumps({"candidate_source":"e947eb7f", "candidate_version":a.version,
        "platform":platform.platform(), "machine":platform.machine(), "python":sys.version,
        "node":shutil.which("node"), "npm":shutil.which("npm"), "workdir":str(work),
        "artifact_sha256":{n:h for n,(_,h) in artifacts.items() if n in selected}},sort_keys=True),flush=True)

    env = os.environ.copy()
    env["ASTER_SMOKE_VERSION"] = a.version
    for key in ("PYTHONPATH", "PYTHONHOME", "NODE_PATH", "ASTER_STORAGE_PATH", "ASTER_SECRET_KEY_FILE", "ASTER_IDENTITY_FILE"):
        env.pop(key, None)

    if a.python:
        pdir=work/"python"; pdir.mkdir(); venv=pdir/"venv"
        (pdir/"consumer.py").write_text(PYTHON_APP)
        run([sys.executable,"-m","venv",str(venv)],cwd=pdir,env=env,log_path=work/"python-venv.log")
        py=venv/"bin"/"python"
        wheel=artifacts["wheel"][0]
        with zipfile.ZipFile(wheel) as archive:
            members=[name for name in archive.namelist() if name.startswith("aster/_aster.") and name.endswith(".so")]
            if len(members) != 1:
                raise SystemExit(f"expected exactly one Linux native module in wheel: {members}")
            env["ASTER_SMOKE_PY_NATIVE_SHA256"] = hashlib.sha256(archive.read(members[0])).hexdigest()
        run([str(py),"-m","pip","install",str(wheel)],cwd=pdir,env=env,log_path=work/"python-install.log")
        run([str(py),"consumer.py"],cwd=pdir,env=env,log_path=work/"python-smoke.log")

    if a.typescript:
        if not shutil.which("node") or not shutil.which("npm"):
            raise SystemExit("--typescript requires node and npm on PATH")
        tdir=work/"typescript"; tdir.mkdir()
        deps={"@aster-rpc/aster":"file:"+str(artifacts["npm-aster"][0]),
            "@aster-rpc/transport":"file:"+str(artifacts["npm-transport"][0]),
            "@aster-rpc/transport-linux-x64-gnu":"file:"+str(artifacts["npm-linux-x64-gnu"][0]),
            "@types/node":"24.10.1",
            "typescript":"5.9.2"}
        (tdir/"package.json").write_text(json.dumps({"name":"aster-clean-consumer","private":True,"type":"module","dependencies":deps},indent=2)+"\n")
        (tdir/"tsconfig.json").write_text(json.dumps({"compilerOptions":{"target":"ES2022","module":"NodeNext","moduleResolution":"NodeNext","types":["node"],"skipLibCheck":True,"outDir":"dist","rootDir":"."},"include":["consumer.ts","aster-rpc.generated.ts"]},indent=2)+"\n")
        (tdir/"consumer.ts").write_text(TS_APP)
        run(["npm","install","--no-audit","--no-fund"],cwd=tdir,env=env,log_path=work/"npm-install.log")
        run(["npm","ls","--all","@aster-rpc/aster","@aster-rpc/transport","@aster-rpc/transport-linux-x64-gnu"],cwd=tdir,env=env,log_path=work/"npm-tree.log")
        run(["node_modules/.bin/aster-gen"],cwd=tdir,env=env,log_path=work/"typescript-gen.log")
        run(["node_modules/.bin/tsc"],cwd=tdir,env=env,log_path=work/"typescript-build.log")
        env["ASTER_CONSUMER_ROOT"] = str(tdir)
        run(["node","consumer.js"],cwd=tdir/"dist",env=env,log_path=work/"typescript-smoke.log")
    print(f"CLEAN_CONSUMER_PASS report_dir={work}",flush=True)


if __name__ == "__main__":
    main()

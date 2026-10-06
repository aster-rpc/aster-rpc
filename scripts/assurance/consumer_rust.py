#!/usr/bin/env python3
"""Clean normalized Rust source-crate consumer through a loopback sparse overlay.

Preparation and compilation happen only when this helper is explicitly run.
No publish endpoint exists. Six candidate archives and the exact 22-crate Salvo
wave are mandatory; older registry Salvo versions are never a fallback.
"""
from __future__ import annotations

import argparse
import hashlib
import http.server
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tarfile
import threading
import tomllib
from urllib.error import HTTPError
from urllib.request import urlopen

ASTER_INDEX = "sparse+https://forge.emrul.dev/api/packages/Aster/cargo/"
ASTER_HTTP = ASTER_INDEX.removeprefix("sparse+").rstrip("/")
CRATES_IO = "https://github.com/rust-lang/crates.io-index"
CANDIDATE = ("aster_transport_core", "aster-otel", "aster-macros", "aster-expose", "aster", "aster-transport-salvo")

APP = r'''use std::{sync::atomic::{AtomicUsize, Ordering}, time::Duration};
use aster::{AsterConfig, Node, RelayMode, Result};
use aster::rpc::{async_trait, AsterServer, ResponseSink, StatusCode};
use fory_derive::ForyStruct;
static STARTED: AtomicUsize = AtomicUsize::new(0);
static CANCELLED: AtomicUsize = AtomicUsize::new(0);
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
struct HoldGuard;
impl Drop for HoldGuard { fn drop(&mut self) { ACTIVE.fetch_sub(1, Ordering::SeqCst); CANCELLED.fetch_add(1, Ordering::SeqCst); } }
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "assurance/Payload")]
pub struct Payload { pub message: String, pub count: i32, #[fory(bytes)] pub data: Vec<u8> }
#[aster::service(name = "ConsumerEcho", version = 1)]
pub trait ConsumerEcho {
    async fn echo(&self, req: Payload) -> Result<Payload>;
    #[rpc(server_stream)] async fn generate(&self, req: Payload, out: ResponseSink<Payload>) -> Result<()>;
    async fn hold(&self, req: Payload) -> Result<Payload>;
}
struct Echo;
#[async_trait]
impl ConsumerEcho for Echo {
    async fn echo(&self, mut req: Payload) -> Result<Payload> { req.count += 1; Ok(req) }
    async fn generate(&self, req: Payload, out: ResponseSink<Payload>) -> Result<()> {
        for i in 0..req.count { let mut r = req.clone(); r.count = i; out.send(&r).await?; } Ok(())
    }
    async fn hold(&self, _req: Payload) -> Result<Payload> {
        STARTED.fetch_add(1, Ordering::SeqCst); ACTIVE.fetch_add(1, Ordering::SeqCst);
        let _guard = HoldGuard; std::future::pending().await
    }
}
fn cfg() -> AsterConfig { AsterConfig::builder().relay(RelayMode::Disabled).bind_addr("127.0.0.1:0").build() }
async fn wait_for(counter: &AtomicUsize, value: usize) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        while counter.load(Ordering::SeqCst) < value { tokio::time::sleep(Duration::from_millis(10)).await; }
    }).await?;
    Ok(())
}
async fn smoke() -> anyhow::Result<()> {
    assert_eq!(aster::VERSION, env!("ASTER_CONSUMER_VERSION"));
    let server = AsterServer::builder().config(cfg()).service(ConsumerEchoServer::new(Echo)).builtin_node_info(false).start().await?;
    let node = match Node::start(cfg()).await {
        Ok(node) => node,
        Err(error) => { tokio::time::timeout(Duration::from_secs(6), server.shutdown()).await?; return Err(error.into()); }
    };
    let result = async {
        for _ in 0..100 { if !server.node().addr().direct_addresses.is_empty() { break; } tokio::time::sleep(Duration::from_millis(20)).await; }
        node.add_peer(server.node())?; server.node().add_peer(&node)?;
        let endpoint: aster::native::iroh::Endpoint = node.native_endpoint();
        anyhow::ensure!(endpoint.id().to_string() == node.id().to_string(), "native handle identity mismatch");
        let _: aster::native::iroh_blobs::api::Store = node.blobs().native_store();
        let _: aster::native::iroh_docs::protocol::Docs = node.docs().native_docs();
        let _: aster::native::iroh_gossip::net::Gossip = node.gossip().native_gossip();
        let expose = node.expose(); expose.unexpose("consumer-unused");
        let _routes = aster_transport_salvo::salvo::Router::new().push(aster_transport_salvo::static_router("/unused", "."));
        let conn = tokio::time::timeout(Duration::from_secs(8), node.rpc_connect(&server.id())).await??;
        let client = ConsumerEchoClient::new(conn.clone().with_deadline(Duration::from_secs(2)));
        let payload = Payload { message: "candidate".into(), count: 41, data: (0..16384).map(|i| ((i*131+i/8)%251) as u8).collect() };
        let reply = client.echo(payload.clone()).await?;
        anyhow::ensure!(reply.message == payload.message && reply.count == 42 && reply.data == payload.data, "unary integrity mismatch");
        let mut req = payload.clone(); req.count = 5;
        let mut stream = client.generate(req.clone()).await?; let mut count = 0;
        while let Some(reply) = stream.recv().await { let reply = reply?; anyhow::ensure!(reply.count == count && reply.message == req.message && reply.data == req.data, "stream integrity mismatch"); count += 1; }
        anyhow::ensure!(count == 5, "stream length mismatch");
        let deadline = ConsumerEchoClient::new(conn.clone().with_deadline(Duration::from_secs(1)));
        let error = deadline.hold(payload.clone()).await.err().ok_or_else(|| anyhow::anyhow!("hold deadline unexpectedly succeeded"))?;
        anyhow::ensure!(error.rpc_code() == Some(StatusCode::DeadlineExceeded), "deadline status mismatch: {error}"); wait_for(&CANCELLED, 1).await?;
        let caller = ConsumerEchoClient::new(conn.with_deadline(Duration::from_secs(10)));
        let mut call = Box::pin(caller.hold(payload));
        tokio::select! { finished = &mut call => anyhow::bail!("hold ended before caller drop: {finished:?}"), ready = wait_for(&STARTED, 2) => { ready?; } }
        drop(call); wait_for(&CANCELLED, 2).await?; anyhow::ensure!(ACTIVE.load(Ordering::SeqCst) == 0, "native hold handler did not drain");
        Ok::<(), anyhow::Error>(())
    }.await;
    let client_shutdown = tokio::time::timeout(Duration::from_secs(6), node.shutdown()).await;
    let server_shutdown = tokio::time::timeout(Duration::from_secs(6), server.shutdown()).await;
    client_shutdown?; server_shutdown?; result?;
    println!("{}", serde_json::json!({"phase":"rust_result", "result":"PASS", "version":aster::VERSION, "counts":{"unary":1,"stream":5,"deadline":1,"cancel":1,"shutdown":1}, "active_holds":ACTIVE.load(Ordering::SeqCst)}));
    Ok(())
}
#[tokio::main]
async fn main() -> anyhow::Result<()> { tokio::time::timeout(Duration::from_secs(40), smoke()).await? }
'''


def sha(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""): h.update(block)
    return h.hexdigest()


def index_path(name):
    n = name.lower()
    return f"{n[:2]}/{n[2:4]}/{n}" if len(n) > 3 else f"3/{n[0]}/{n}" if len(n) == 3 else f"{len(n)}/{n}"


def normalized_archive(path, name, version, native_names):
    """Read only Cargo's normalized manifest; do not synthesize from checkout metadata."""
    with tarfile.open(path, "r:gz") as archive:
        members = archive.getmembers()
        prefix = f"{name}-{version}/"
        assert all((m.name.startswith(prefix) or (m.isdir() and m.name.rstrip("/")==prefix.rstrip("/")))
                   and not m.issym() and not m.islnk() and ".." not in Path(m.name).parts for m in members), path
        assert sum(m.size for m in members)<=256*1024*1024, "unpacked archive exceeds bound"
        manifest = tomllib.loads(archive.extractfile(prefix + "Cargo.toml").read().decode())
        assert archive.getmember(prefix + "Cargo.toml.orig").isfile(), path
    package = manifest["package"]
    assert package["name"] == name and package["version"] == version, (path, package)
    assert not any(k in manifest for k in ("workspace", "patch", "replace")), path
    deps = []
    def section(values, kind, target=None):
        for alias, value in values.items():
            dep = {"version":value} if isinstance(value, str) else value
            assert not any(k in dep for k in ("path", "git", "workspace")), (path, alias, dep)
            actual = dep.get("package", alias)
            source = dep.get("registry-index", dep.get("registry", CRATES_IO))
            assert source != "aster", (path, "unnormalized registry alias", alias)
            if actual in native_names or actual in CANDIDATE:
                assert source.rstrip("/") == ASTER_INDEX.rstrip("/"), (path, "native dependency lost canonical registry", actual, source)
            deps.append(dict(name=alias, package=actual if actual != alias else None,
                             req=dep["version"], features=dep.get("features", []),
                             optional=dep.get("optional", False), default_features=dep.get("default-features", True),
                             kind=kind, target=target, registry=None if source.rstrip("/") == ASTER_INDEX.rstrip("/") else source))
    for section_name, kind in (("dependencies","normal"),("build-dependencies","build"),("dev-dependencies","dev")):
        section(manifest.get(section_name, {}), kind)
        for target, tables in manifest.get("target", {}).items(): section(tables.get(section_name, {}), kind, target)
    entry = dict(name=name, vers=version, deps=deps, cksum=sha(path), features={},
                 features2=manifest.get("features", {}), v=2, yanked=False,
                 links=package.get("links"), rust_version=package.get("rust-version"))
    return entry, manifest


def publisher(root):
    spec = importlib.util.spec_from_file_location("assurance_native_publisher", root / "scripts/release/publish-native-stack.py")
    module = importlib.util.module_from_spec(spec); sys.modules[spec.name] = module; spec.loader.exec_module(module)
    return module


def run(command, cwd, env, log, timeout):
    print("$", " ".join(command), flush=True)
    with log.open("w") as stream:
        process=subprocess.Popen(command,cwd=cwd,env=env,stdout=stream,stderr=subprocess.STDOUT,start_new_session=True)
        try: code=process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=5);raise
        if code:raise subprocess.CalledProcessError(code,command)


def isolated_env(output):
    env = os.environ.copy()
    for key in list(env):
        if key.startswith(("CARGO_", "ASTER_", "OTEL_", "RUSTFLAGS", "RUSTC_WRAPPER", "SCCACHE_")) or key in ("LD_PRELOAD", "LD_LIBRARY_PATH"):
            env.pop(key, None)
    env.update(CARGO_HOME=str(output / "cargo-home"), CARGO_TARGET_DIR=str(output / "target"))
    return env


def prepare_salvo(source, output, module, bom, timeout):
    """Copy an exact cached checkout; rewrite dependency metadata only; never publish."""
    expected = bom["forks"]["salvo"]["commit"]
    actual = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    assert actual == expected, (actual, expected)
    assert not subprocess.check_output(["git", "-C", str(source), "status", "--porcelain", "--untracked-files=no"], text=True).strip(), "cached fork checkout is dirty"
    staging = output / "salvo-source"
    shutil.copytree(source, staging, ignore=shutil.ignore_patterns(".git", "target"))
    packages = [p for p in module.PACKAGES if p.source == "salvo" and p.kind == "fork"]
    before = {p.name: module.source_tree_digests((staging / p.manifest).parent) for p in packages}
    module.patch_tree(staging)
    for p in packages: assert before[p.name] == module.source_tree_digests((staging / p.manifest).parent), p.name
    config = staging / ".cargo"; config.mkdir(exist_ok=True)
    (config / "config.toml").write_text(f'[registries.aster]\nindex = "{ASTER_INDEX}"\n')
    command = ["cargo", "package", "--registry", "aster", "--allow-dirty", "--no-verify"]
    for p in packages: command.extend(["-p", p.name])
    env = isolated_env(output)
    run(command, staging, env, output / "salvo-package.log", timeout)
    destination = output / "salvo-archives"; destination.mkdir()
    for p in packages: shutil.copy2(output / "target/package" / f"{p.name}-{p.version}.crate", destination)
    (destination / "provenance.json").write_text(json.dumps(dict(source_commit=actual, metadata_only_rewrite=True, archive_sha256={p.name:sha(destination / f"{p.name}-{p.version}.crate") for p in packages}), indent=2)+"\n")
    return destination


class Overlay:
    def __init__(self, local, allowed_remote, output):
        self.local, self.allowed_remote, self.output = local, allowed_remote, output
        self.requests = set(); self.lock = threading.Lock()
        self.remote_entries = {}; self.archive_locks = {name:threading.Lock() for name in allowed_remote}
        (output/"remote-archives").mkdir()
        owner = self
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_): pass
            def do_GET(self):
                route = self.path.split("?", 1)[0]
                try:
                    if route == "/config.json":
                        raw = json.dumps({"dl":owner.url+"/archives/{crate}/{version}", "auth-required":False}).encode()
                    elif route.startswith("/archives/"):
                        _, _, name, version = route.split("/")
                        item = owner.local.get(name)
                        if item and version == item[0]["vers"]: archive_path = item[1]
                        elif name in owner.allowed_remote and version == owner.allowed_remote[name]:
                            archive_path=owner.output/"remote-archives"/f"{name}-{version}.crate"
                            with owner.archive_locks[name]:
                                if not archive_path.exists():
                                    entry=owner.remote_entry(name)
                                    temporary=archive_path.with_suffix(".partial")
                                    with urlopen(f"{ASTER_HTTP}/api/v1/crates/{name}/{version}/download",timeout=30) as response, temporary.open("wb") as outgoing:
                                        total=0
                                        while block:=response.read(1024*1024):
                                            total+=len(block);assert total<=128*1024*1024,"remote archive exceeds bound";outgoing.write(block)
                                    assert sha(temporary)==entry["cksum"],(name,"public archive checksum mismatch")
                                    temporary.rename(archive_path)
                        else: raise HTTPError(route,404,"version outside frozen wave",{},None)
                        with owner.lock: owner.requests.add(route)
                        self.send_response(200);self.send_header("Content-Length",str(archive_path.stat().st_size));self.end_headers()
                        with archive_path.open("rb") as incoming: shutil.copyfileobj(incoming,self.wfile,length=1024*1024)
                        return
                    else:
                        name = route.rsplit("/",1)[-1]
                        if route != "/"+index_path(name): raise HTTPError(route,404,"unknown index path",{},None)
                        if name in owner.local: rows = [owner.local[name][0]]
                        elif name in owner.allowed_remote:
                            rows=[owner.remote_entry(name)]
                        else: raise HTTPError(route,404,"package outside frozen native wave",{},None)
                        raw=("\n".join(json.dumps(row) for row in rows)+"\n").encode()
                    with owner.lock: owner.requests.add(route)
                    self.send_response(200); self.send_header("Content-Length",str(len(raw))); self.end_headers(); self.wfile.write(raw)
                except HTTPError as error: self.send_error(error.code)
                except Exception as error:
                    with owner.lock:
                        with (owner.output/"overlay-errors.log").open("a") as stream: stream.write(str(error)+"\n")
                    self.send_error(502)
        class BoundedServer(http.server.ThreadingHTTPServer):
            daemon_threads=True
            request_queue_size=128
            permits=threading.BoundedSemaphore(16)
            def process_request(self,request,address):
                if not self.permits.acquire(timeout=30): self.close_request(request);return
                try: super().process_request(request,address)
                except BaseException: self.permits.release();raise
            def process_request_thread(self,request,address):
                try: super().process_request_thread(request,address)
                finally: self.permits.release()
        self.server=BoundedServer(("127.0.0.1",0),Handler)
        self.url=f"http://127.0.0.1:{self.server.server_address[1]}"
        self.thread=threading.Thread(target=self.server.serve_forever,daemon=True);self.thread.start()
    def remote_entry(self,name):
        with self.lock:
            if name in self.remote_entries:return self.remote_entries[name]
        with urlopen(ASTER_HTTP+"/"+index_path(name),timeout=15) as response:data=response.read(16*1024*1024+1)
        assert len(data)<=16*1024*1024
        rows=[json.loads(line) for line in data.decode().splitlines() if line]
        rows=[row for row in rows if row["vers"]==self.allowed_remote[name] and not row.get("yanked",False)]
        assert len(rows)==1,(name,"exact public native wave missing")
        with self.lock:self.remote_entries[name]=rows[0]
        return rows[0]
    def close(self):
        self.server.shutdown();self.server.server_close();self.thread.join(timeout=5)
        (self.output/"overlay-requests.json").write_text(json.dumps(sorted(self.requests),indent=2)+"\n")


def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--checkout-root",type=Path,required=True)
    ap.add_argument("--candidate-dir",type=Path)
    ap.add_argument("--candidate-provenance",type=Path,help="JSON source_commit/version/archive_sha256 frozen after exact candidate packaging")
    ap.add_argument("--salvo-dir",type=Path)
    ap.add_argument("--prepare-salvo-from",type=Path,help="exact clean cached Salvo checkout; preparation happens only on invocation")
    ap.add_argument("--prepare-only",action="store_true")
    ap.add_argument("--version",default="0.3.705")
    ap.add_argument("--candidate-commit",default="e947eb7fb1bef3b5d2978702df08b3f9ebea3d70")
    ap.add_argument("--output",type=Path,required=True)
    ap.add_argument("--timeout",type=int,default=900)
    args=ap.parse_args()
    root=args.checkout_root.resolve(strict=True);output=args.output.resolve()
    assert root not in output.parents and output not in root.parents and output!=root, "consumer output must be outside checkout"
    assert not output.exists() or not any(output.iterdir()), "fresh empty output required"
    output.mkdir(parents=True,exist_ok=True)
    assert re.fullmatch(r"\d+\.\d+\.\d+",args.version) and 1<=args.timeout<=1800
    module=publisher(root);bom=module.load_release_manifest()
    native_versions={p.name:p.version for p in module.PACKAGES}
    salvo=[p for p in module.PACKAGES if p.source=="salvo" and p.kind=="fork"]
    assert len(salvo)==22 and all(p.version=="1.0.2" for p in salvo), "helper is frozen to exact 22-crate Salvo1.0.2 wave"
    if args.prepare_salvo_from:
        assert not args.salvo_dir
        args.salvo_dir=prepare_salvo(args.prepare_salvo_from.resolve(strict=True),output,module,bom,args.timeout)
    if args.prepare_only:
        assert args.prepare_salvo_from;print(json.dumps({"phase":"rust_salvo_prepared","directory":str(args.salvo_dir)}));return
    assert args.candidate_dir and args.candidate_provenance and args.salvo_dir, "six candidate archives/provenance and 22 exact Salvo archives required"
    provenance=json.loads(args.candidate_provenance.read_text())
    assert provenance["source_commit"]==args.candidate_commit and provenance["version"]==args.version
    salvo_provenance=json.loads((args.salvo_dir/"provenance.json").read_text())
    assert salvo_provenance["source_commit"]==bom["forks"]["salvo"]["commit"] and salvo_provenance["metadata_only_rewrite"] is True
    local={}
    for name,version,directory,proof in [(n,args.version,args.candidate_dir,provenance) for n in CANDIDATE]+[(p.name,p.version,args.salvo_dir,salvo_provenance) for p in salvo]:
        path=(directory/f"{name}-{version}.crate").resolve(strict=True)
        assert sha(path)==proof["archive_sha256"][name], (name,"archive provenance checksum mismatch")
        entry,_=normalized_archive(path,name,version,set(native_versions));local[name]=(entry,path)
    remote={n:v for n,v in native_versions.items() if n not in local}
    assert len(local)==28 and len(remote)==20
    overlay=Overlay(local,remote,output)
    try:
        project=output/"consumer"; (project/"src").mkdir(parents=True);(project/".cargo").mkdir()
        (project/"src/main.rs").write_text(APP)
        (project/"Cargo.toml").write_text(f'''[package]
name = "assurance-clean-rust-consumer"
version = "0.0.0"
edition = "2021"
[dependencies]
aster = {{ version = "={args.version}", registry = "aster", features = ["rpc", "expose", "expose-edge", "otel"] }}
aster-transport-salvo = {{ version = "={args.version}", registry = "aster" }}
fory-core = "=1.7.6"
fory-derive = "=1.7.6"
anyhow = "1"
serde_json = "1"
tokio = {{ version = "1", features = ["full"] }}
''')
        (project/".cargo/config.toml").write_text(f'''[registries.aster]
index = "{ASTER_INDEX}"
[source.aster]
registry = "{ASTER_INDEX}"
replace-with = "assurance-overlay"
[source.assurance-overlay]
registry = "sparse+{overlay.url}/"
''')
        env=isolated_env(output);env["ASTER_CONSUMER_VERSION"]=args.version
        with (output/"metadata.json").open("w") as stdout, (output/"metadata.stderr.log").open("w") as stderr:
            command=["cargo","metadata","--format-version","1"]
            process=subprocess.Popen(command,cwd=project,env=env,stdout=stdout,stderr=stderr,start_new_session=True)
            try: code=process.wait(timeout=args.timeout)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=5);raise
            if code:raise subprocess.CalledProcessError(code,command)
        parsed=json.loads((output/"metadata.json").read_text()); packages=parsed["packages"]
        ids={}
        for package in packages:
            if package["name"]=="assurance-clean-rust-consumer":continue
            source=package.get("source")
            assert source and source.startswith(("registry+", "sparse+")), (package["name"],"consumer path/git fallback",source)
            ids.setdefault(package["name"],set()).add(source)
            if package["name"] in CANDIDATE or package["name"] in native_versions:
                expected=args.version if package["name"] in CANDIDATE else native_versions[package["name"]]
                assert package["version"]==expected and ASTER_HTTP in source, (package["name"],package["version"],source)
                origin=Path(package["manifest_path"]).resolve()
                assert (output/"cargo-home").resolve() in origin.parents, (package["name"],"package escaped isolated Cargo cache",origin)
        for name in set(CANDIDATE)|set(native_versions):assert len(ids.get(name,set()))<=1, (name,"duplicate native source identity",ids.get(name))
        assert all(name in ids for name in CANDIDATE)
        for name in ("fory-core","fory-derive"):
            pair=[package for package in packages if package["name"]==name]
            assert len(pair)==1 and pair[0]["version"]=="1.7.6" and CRATES_IO in pair[0]["source"], (name,"Fory source/version split",pair)
        lock=tomllib.loads((project/"Cargo.lock").read_text())
        for package in lock["package"]:
            name=package["name"]
            if name in CANDIDATE or name in native_versions:
                expected=args.version if name in CANDIDATE else native_versions[name]
                assert package["version"]==expected and ASTER_HTTP in package.get("source",""), (name,"native lock source/version mismatch",package)
                if name in local:assert package["checksum"]==local[name][0]["cksum"],package
                elif name in remote:assert package["checksum"]==overlay.remote_entry(name)["cksum"],package
        run(["cargo","run","--locked"],project,env,output/"consumer.log",args.timeout)
        receipts=[]
        for line in (output/"consumer.log").read_text(errors="replace").splitlines():
            try: value=json.loads(line)
            except json.JSONDecodeError: continue
            if isinstance(value,dict) and value.get("phase")=="rust_result":receipts.append(value)
        assert len(receipts)==1 and receipts[0].get("result")=="PASS" and receipts[0].get("version")==args.version and receipts[0].get("active_holds")==0, receipts
        result={"phase":"rust_consumer","result":"PASS","scope":"normalized_source_crates_loopback_sparse_overlay","public_registry_proof":False,
                "version":args.version,"candidate_commit":args.candidate_commit,"local_archives":28,"native_registry_proxies":20,
                "archive_sha256":{name:row[0]["cksum"] for name,row in local.items()},"consumer_lock_sha256":sha(project/"Cargo.lock"),"project":str(project)}
    finally:overlay.close()
    (output/"result.json").write_text(json.dumps(result,indent=2)+"\n");print(json.dumps(result),flush=True)


if __name__=="__main__":main()

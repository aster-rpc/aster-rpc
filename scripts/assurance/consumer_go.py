#!/usr/bin/env python3
"""Linux Go staged source-package smoke; deliberately NOT public module registry proof.

The candidate module identity is sdk.getaster.now/go. A fresh consumer uses
an explicit local replace to an extracted, hashed archive, plus the hashed
candidate C header/native library. Invocation builds only this consumer, not the
Rust SDK.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile


APP = r'''package main

/*
#include "aster_ffi.h"
*/
import "C"
import (
  "bytes"
  "context"
  "crypto/sha256"
  "encoding/hex"
  "encoding/json"
  "errors"
  "fmt"
  "os"
  "path/filepath"
  "runtime/debug"
  "strings"
  "sync/atomic"
  "time"
  aster "sdk.getaster.now/go"
)

type EchoRequest struct { Message string; Count int32; Data []byte `fory:"type=bytes"` }
func(EchoRequest) AsterWireType() string { return "interop/EchoRequest" }
type EchoReply struct { Message string; Count int32; Data []byte `fory:"type=bytes"`; ServerLang string }
func(EchoReply) AsterWireType() string { return "interop/EchoReply" }

func check(ok bool, text string) { if !ok { panic(text) } }
func must(err error) { if err != nil { panic(err) } }
func receipt(value any) { must(json.NewEncoder(os.Stdout).Encode(value)) }
func wait(counter *atomic.Int32, target int32, what string) {
  until:=time.Now().Add(3*time.Second)
  for counter.Load()<target { check(time.Now().Before(until),"timed out: "+what); time.Sleep(10*time.Millisecond) }
}
func boundedClose(close func() error) {
  done:=make(chan error,1); go func(){ done<-close() }()
  select { case err:=<-done: must(err); case <-time.After(6*time.Second): panic("shutdown exceeded 6 s") }
}
func main() {
  native,err:=filepath.EvalSymlinks(os.Getenv("ASTER_CONSUMER_FFI_PATH")); must(err)
  raw,err:=os.ReadFile(native); must(err); digest:=sha256.Sum256(raw)
  check(hex.EncodeToString(digest[:])==os.Getenv("ASTER_CONSUMER_FFI_SHA256"),"candidate native hash mismatch")
  maps,err:=os.ReadFile("/proc/self/maps"); must(err)
  loaded:=map[string]bool{}
  for _,line:=range strings.Split(string(maps),"\n") {
    if strings.Contains(line,"libaster_transport_ffi.so") {
      fields:=strings.Fields(line); check(len(fields)==6,"unreadable FFI mapping")
      path,err:=filepath.EvalSymlinks(fields[5]); must(err); loaded[path]=true
    }
  }
  check(len(loaded)==1 && loaded[native],"loaded FFI did not come from staged native directory")
  version:=C.GoString(C.aster_version())
  check(version==os.Getenv("ASTER_CONSUMER_VERSION"),"native release version mismatch: "+version)
  info,ok:=debug.ReadBuildInfo(); check(ok,"missing Go build provenance")
  moduleDir:=""
  for _,dep:=range info.Deps { if dep.Path=="sdk.getaster.now/go" && dep.Replace!=nil { moduleDir=dep.Replace.Path } }
  check(moduleDir==os.Getenv("ASTER_CONSUMER_MODULE"),"Go build did not use isolated staged module: "+moduleDir)
  major,minor,patch:=aster.Version()
  receipt(map[string]any{"phase":"go_metadata","version":version,"module_identity":"sdk.getaster.now/go","module_origin":moduleDir,"native":native,"native_sha256":hex.EncodeToString(digest[:]),"abi":fmt.Sprintf("%d.%d.%d",major,minor,patch),"go":info.GoVersion,"public_registry_proof":false})
  var started,cancelled atomic.Int32
  service:=aster.NewService("InteropEcho",1,
    aster.Unary("echo",func(_ context.Context,req *EchoRequest)(*EchoReply,error){ return &EchoReply{Message:req.Message,Count:req.Count+1,Data:req.Data,ServerLang:"go"},nil }),
    aster.ServerStream("generate",func(ctx context.Context,req *EchoRequest,send func(*EchoReply)error)error {
      for i:=int32(0);i<req.Count;i++ { if err:=send(&EchoReply{Message:req.Message,Count:i,Data:req.Data,ServerLang:"go"});err!=nil{return err} };return nil
    }),
    aster.Unary("hold",func(ctx context.Context,req *EchoRequest)(*EchoReply,error){
      started.Add(1)
      select { case <-ctx.Done():cancelled.Add(1);return nil,ctx.Err();case <-time.After(30*time.Second):return nil,fmt.Errorf("hold unexpectedly finished") }
    }),
  )
  ctx,stop:=context.WithTimeout(context.Background(),40*time.Second);defer stop()
  cfg:=aster.AsterConfig{RelayMode:"disabled",BindAddr:"127.0.0.1:0"}
  server,err:=aster.StartServer(ctx,aster.ServerOptions{Config:cfg,Services:[]*aster.Service{service}});must(err);defer server.Close()
  client,err:=aster.StartClient(ctx,cfg);must(err);defer client.Close();must(client.RegisterService(service))
  ticket,err:=server.Ticket(ctx);must(err)
  dial,stopDial:=context.WithTimeout(ctx,8*time.Second);conn,err:=client.ConnectTicket(dial,ticket);stopDial();must(err)
  data:=[]byte{0,1,2,255};request:=&EchoRequest{Message:"candidate",Count:41,Data:data}
  unary,stopUnary:=context.WithTimeout(ctx,4*time.Second)
  reply,err:=aster.Call[EchoRequest,EchoReply](unary,conn,"InteropEcho","echo",request);stopUnary();must(err)
  check(reply.Message==request.Message && reply.Count==42 && bytes.Equal(reply.Data,data) && reply.ServerLang=="go","unary integrity")
  stream,stopStream:=context.WithTimeout(ctx,4*time.Second);count:=int32(0)
  for reply,err:=range aster.CallServerStream[EchoRequest,EchoReply](stream,conn,"InteropEcho","generate",&EchoRequest{Message:"stream",Count:5,Data:data}) {
    must(err);check(reply.Message=="stream" && reply.Count==count && bytes.Equal(reply.Data,data) && reply.ServerLang=="go","stream integrity");count++
  };stopStream();check(count==5,"stream count")
  deadline,stopDeadline:=context.WithTimeout(ctx,time.Second);beginning:=time.Now()
  _,err=aster.Call[EchoRequest,EchoReply](deadline,conn,"InteropEcho","hold",request);stopDeadline()
  var rpc *aster.RpcError
  check(errors.As(err,&rpc) && rpc.Code==aster.StatusDeadlineExceeded,fmt.Sprintf("wrong deadline error: %T %v",err,err))
  check(time.Since(beginning)>=400*time.Millisecond && time.Since(beginning)<3*time.Second,"deadline time bound");wait(&cancelled,1,"deadline handler cancellation")
  callCtx,stopBudget:=context.WithTimeout(ctx,10*time.Second);defer stopBudget()
  callCtx,cancel:=context.WithCancel(callCtx);defer cancel();done:=make(chan error,1)
  go func(){_,err:=aster.Call[EchoRequest,EchoReply](callCtx,conn,"InteropEcho","hold",request);done<-err}()
  wait(&started,2,"caller cancel handler start");cancel()
  select { case err:=<-done:check(errors.Is(err,context.Canceled)||errors.Is(err,aster.ErrCancelled),fmt.Sprintf("wrong caller cancellation: %v",err));case <-time.After(2*time.Second):panic("caller cancellation exceeded bound") }
  wait(&cancelled,2,"caller cancellation reaching handler")
  boundedClose(client.Close);boundedClose(server.Close)
  _,err=aster.ShutdownObservability(2*time.Second);must(err)
  receipt(map[string]any{"phase":"go_result","result":"PASS","counts":map[string]int{"unary":1,"stream":5,"deadline":1,"cancel":1,"shutdown":1}})
}
'''


def sha(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def run(command: list[str], cwd: Path, env: dict[str, str], log: Path, timeout: int = 240) -> str:
    print("$", " ".join(command), flush=True)
    with log.open("w") as out:
        result = subprocess.run(command, cwd=cwd, env=env, stdout=out, stderr=subprocess.STDOUT, timeout=timeout)
    text = log.read_text(errors="replace")
    print(text[-16000:], end="", flush=True)
    if result.returncode:
        raise SystemExit(f"command failed ({result.returncode}); full log: {log}")
    return text


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    for name in ("module-archive", "ffi", "header"):
        ap.add_argument(f"--{name}", type=Path, required=True)
        ap.add_argument(f"--{name}-sha256", required=True)
    ap.add_argument("--version", required=True)
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--checkout-root", type=Path)
    args = ap.parse_args()
    if not re.fullmatch(r"\d+\.\d+\.\d+", args.version):
        ap.error("exact numeric release version required")
    if platform.system() != "Linux" or platform.machine().lower() not in ("x86_64", "amd64"):
        ap.error("this smoke is scoped to Linux x86_64")
    artifacts: dict[str, Path] = {}
    for name in ("module_archive", "ffi", "header"):
        path = getattr(args, name).resolve(strict=True)
        expected = getattr(args, name + "_sha256")
        if not re.fullmatch(r"[0-9a-fA-F]{64}", expected) or sha(path) != expected.lower():
            ap.error(f"candidate SHA-256 mismatch for {name}: {path}")
        artifacts[name] = path
    if artifacts["ffi"].name != "libaster_transport_ffi.so" or artifacts["header"].name != "aster_ffi.h":
        ap.error("expected libaster_transport_ffi.so and aster_ffi.h")
    if not shutil.which("go"):
        ap.error("Go 1.25+ and a C compiler are required")
    checkout = args.checkout_root.resolve(strict=True) if args.checkout_root else next((p for p in Path(__file__).resolve().parents if (p / ".git").exists()), None)
    if checkout is None:
        ap.error("relocated helper requires --checkout-root")
    output = args.output.resolve()
    if output == checkout or checkout in output.parents or output in checkout.parents:
        ap.error("--output must be outside checkout")
    if output.exists() and any(output.iterdir()):
        ap.error("--output must be fresh and empty")
    output.mkdir(parents=True)
    project = Path(tempfile.mkdtemp(prefix="consumer-", dir=output))
    source = output / "staged-module"
    source.mkdir()
    with tarfile.open(artifacts["module_archive"]) as archive:
        for member in archive.getmembers():
            target = (source / member.name).resolve()
            if source not in target.parents and target != source:
                ap.error(f"archive path escaped staging: {member.name}")
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
            elif member.isfile():
                target.parent.mkdir(parents=True, exist_ok=True)
                with archive.extractfile(member) as incoming, target.open("wb") as outgoing:
                    shutil.copyfileobj(incoming, outgoing)
            else:
                ap.error(f"archive must contain regular files/directories only: {member.name}")
    modules = list(source.rglob("go.mod"))
    if len(modules) != 1:
        ap.error(f"archive must contain exactly one candidate go.mod: {modules}")
    module = modules[0].parent
    text = modules[0].read_text()
    if not re.search(r"(?m)^module\s+" + re.escape("sdk.getaster.now/go") + r"\s*$", text) or not (module / "go.sum").is_file():
        ap.error("archive must be the sdk.getaster.now/go source package with go.sum")
    if re.search(r"(?m)^replace\s", text):
        ap.error("staged module cannot refer to additional local source replacements")
    native_dir, include_dir = output / "native", output / "include"
    native_dir.mkdir(); include_dir.mkdir()
    native = native_dir / "libaster_transport_ffi.so"
    shutil.copy2(artifacts["ffi"], native)
    shutil.copy2(artifacts["header"], include_dir / "aster_ffi.h")
    (project / "main.go").write_text(APP)
    (project / "go.mod").write_text(f'module assurance-clean-consumer\n\ngo 1.25.0\n\nrequire sdk.getaster.now/go v0.0.0\n\nreplace sdk.getaster.now/go => {json.dumps(str(module))}\n')
    env = os.environ.copy()
    for key in list(env):
        if key.startswith(("ASTER_", "OTEL_")):
            env.pop(key, None)
    for key in ("GOFLAGS", "CGO_CFLAGS", "CGO_CPPFLAGS", "CGO_CXXFLAGS", "CGO_LDFLAGS", "LD_PRELOAD", "LD_LIBRARY_PATH", "GOPATH", "GOMODCACHE", "GOCACHE", "ASTER_STORAGE_PATH", "ASTER_SECRET_KEY_FILE", "ASTER_IDENTITY_FILE"):
        env.pop(key, None)
    # Go parses these variables itself rather than through a shell. Literal
    # quotes therefore become invalid cgo directives even when the path has no
    # spaces. This isolated Linux output path is deliberately space-free.
    env.update(GOENV="off", GOWORK="off", CGO_ENABLED="1", GOPATH=str(output / "gopath"), GOMODCACHE=str(output / "gomodcache"), GOCACHE=str(output / "gocache"),
               CGO_CFLAGS=f'-I{include_dir}', CGO_LDFLAGS=f'-L{native_dir} -laster_transport_ffi -Wl,-rpath,{native_dir}', LD_LIBRARY_PATH=str(native_dir),
               ASTER_CONSUMER_VERSION=args.version, ASTER_CONSUMER_MODULE=str(module), ASTER_CONSUMER_FFI_PATH=str(native), ASTER_CONSUMER_FFI_SHA256=sha(native))
    metadata = {"scope": "staged_go_source_package_local_replace", "public_registry_proof": False, "version": args.version,
                "module_identity": "sdk.getaster.now/go", "module_origin": str(module), "artifact_sha256": {name: sha(path) for name, path in artifacts.items()}, "project": str(project)}
    print(json.dumps(metadata, sort_keys=True), flush=True)
    run(["go", "mod", "tidy"], project, env, output / "resolve.log")
    resolved = json.loads(run(["go", "list", "-m", "-json", "sdk.getaster.now/go"], project, env, output / "module-origin.log"))
    if resolved.get("Path") != "sdk.getaster.now/go" or Path(resolved["Dir"]).resolve() != module:
        raise SystemExit(f"Go module escaped isolated staged archive: {resolved}")
    executable = project / "consumer"
    run(["go", "build", "-o", str(executable), "."], project, env, output / "compile.log")
    text = run([str(executable)], project, env, output / "consumer.log", 60)
    if not any(json.loads(line).get("phase") == "go_result" and json.loads(line).get("result") == "PASS" for line in text.splitlines() if line.startswith("{")):
        raise SystemExit("consumer did not emit its success receipt")
    metadata["result"] = "PASS"
    (output / "result.json").write_text(json.dumps(metadata, indent=2) + "\n")
    print(f"GO_STAGED_SOURCE_CONSUMER_PASS report_dir={output}", flush=True)


if __name__ == "__main__":
    main()

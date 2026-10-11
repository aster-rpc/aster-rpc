#!/usr/bin/env python3
"""Verify a released public Go module, independently via proxy and direct VCS.

Example: public_go.py --version 0.3.123 --native-archive natives.tar.gz
  --native-archive-sha256 <hex> --output /tmp/aster-public-proof
Never accepts a local module replacement. Archive download/authentication is external.
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

MODULE = 'sdk.getaster.now/go'
APP = r'''package main
/*
#define _GNU_SOURCE
#include "aster_ffi.h"
#include <dlfcn.h>
static const char *loaded_native(void) {
  Dl_info info;
  if (!dladdr((void *)&aster_version, &info)) return "";
  return info.dli_fname;
}
*/
import "C"
import (
 "context"
 "encoding/json"
 "fmt"
 "os"
 "path/filepath"
 "runtime/debug"
 aster "sdk.getaster.now/go"
)
func main() {
 fail:=func(err error){if err!=nil {panic(err)}}
 fail(aster.CheckABI())
 if C.aster_abi_version()!=C.ASTER_ABI_VERSION {panic("header/native ABI mismatch")}
 version:=C.GoString(C.aster_version())
 if version!=os.Getenv("PROOF_VERSION") {panic("native version mismatch: "+version)}
 loaded,err:=filepath.EvalSymlinks(C.GoString(C.loaded_native()));fail(err)
 expected,err:=filepath.EvalSymlinks(os.Getenv("PROOF_NATIVE"));fail(err)
 if loaded!=expected {panic("unexpected loaded native: "+loaded)}
 info,ok:=debug.ReadBuildInfo();if !ok {panic("missing build info")}
 found:=false
 for _,dep:=range info.Deps {if dep.Path=="sdk.getaster.now/go" {
  if dep.Version!="v"+version || dep.Replace!=nil {panic("incorrect module build provenance")};found=true
 }}
 if !found {panic("module missing from binary provenance")}
 runtime,err:=aster.NewRuntime(context.Background(),aster.DefaultRuntimeConfig());fail(err);fail(runtime.Close())
 major,minor,patch:=aster.Version()
 fail(json.NewEncoder(os.Stdout).Encode(map[string]any{"result":"PASS","version":version,"loaded_native":loaded,"abi":fmt.Sprintf("%d.%d.%d",major,minor,patch),"runtime_create_close":true}))
}
'''

def sha(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()

def clean_env(root: Path, mode: str, native: Path, header: Path, version: str):
    if mode not in ('proxy', 'direct'):
        raise ValueError('unknown resolution mode')
    # A new HOME prevents ~/.netrc, SSH keys and user Git configuration being used.
    env = {key: os.environ[key] for key in ('PATH', 'SYSTEMROOT', 'SDKROOT', 'DEVELOPER_DIR') if key in os.environ}
    env.update(HOME=str(root/'home'), TMPDIR=str(root/'tmp'), GOENV='off', GOWORK='off',
        GOPATH=str(root/'gopath'), GOMODCACHE=str(root/'gomodcache'), GOCACHE=str(root/'gocache'),
        GOPROXY='https://proxy.golang.org' if mode=='proxy' else 'direct',
        GOPRIVATE='', GONOPROXY='', GONOSUMDB='', GOSUMDB='sum.golang.org',
        GOVCS='public:git,private:off', GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=os.devnull,
        GIT_TERMINAL_PROMPT='0', GIT_ASKPASS='/usr/bin/false', SSH_ASKPASS='/usr/bin/false',
        GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0='credential.helper', GIT_CONFIG_VALUE_0='',
        CGO_ENABLED='1', CGO_CFLAGS=f'-I{header.parent}',
        CGO_LDFLAGS=f'-L{native.parent} -laster_transport_ffi -Wl,-rpath,{native.parent}' + (' -ldl' if platform.system()=='Linux' else ''),
        PROOF_VERSION=version, PROOF_NATIVE=str(native))
    return env

def validate_provenance(data, version, cache):
    if data.get('Path')!=MODULE or data.get('Version')!='v'+version or 'Replace' in data:
        raise ValueError('module identity/version/replacement provenance mismatch')
    directory=Path(data.get('Dir','')).resolve()
    if not directory.is_relative_to(cache.resolve()) or not directory.is_dir():
        raise ValueError('module did not resolve inside fresh public module cache')
    if not data.get('Sum') or not data.get('GoModSum'):
        raise ValueError('missing authenticated module checksums')

def extract(archive, target):
    with tarfile.open(archive) as source:
        for item in source.getmembers():
            path=(target/item.name).resolve()
            if not path.is_relative_to(target.resolve()) or not (item.isfile() or item.isdir()):
                raise ValueError('native archive contains unsafe paths or links')
            if item.isdir(): path.mkdir(parents=True,exist_ok=True)
            else:
                path.parent.mkdir(parents=True,exist_ok=True)
                with source.extractfile(item) as incoming, path.open('wb') as outgoing:
                    shutil.copyfileobj(incoming,outgoing)

def run(command, cwd, env, log):
    result=subprocess.run(command,cwd=cwd,env=env,capture_output=True,text=True,timeout=300)
    log.write_text(result.stdout+result.stderr)
    if result.returncode: raise RuntimeError(f'command failed: {command}; see {log}')
    return result.stdout

def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--version',required=True)
    ap.add_argument('--native-archive',type=Path,required=True)
    ap.add_argument('--native-archive-sha256',required=True)
    ap.add_argument('--output',type=Path,required=True)
    args=ap.parse_args()
    if not re.fullmatch(r'0\.\d+\.\d+',args.version): ap.error('exact preview version required')
    if platform.system() not in ('Darwin','Linux'): ap.error('currently supports macOS and Linux')
    if sha(args.native_archive)!=args.native_archive_sha256.lower(): ap.error('native archive checksum mismatch')
    output=args.output.resolve()
    if output.exists(): ap.error('output must not exist')
    if any(c.isspace() for c in str(output)): ap.error('output path must not contain whitespace for cgo flags')
    output.mkdir(parents=True)
    staged=output/'native';staged.mkdir();extract(args.native_archive,staged)
    name='libaster_transport_ffi.dylib' if platform.system()=='Darwin' else 'libaster_transport_ffi.so'
    libraries=list(staged.rglob(name));headers=list(staged.rglob('aster_ffi.h'))
    if len(libraries)!=1 or len(headers)!=1: ap.error('archive requires exactly one matching native library and header')
    native,header=libraries[0],headers[0]
    if native.stat().st_size==0 or header.stat().st_size==0: ap.error('native/header must be nonempty')
    receipts=[]
    for mode in ('proxy','direct'):
        root=output/mode;root.mkdir()
        for name in ('home','tmp','project'): (root/name).mkdir()
        project=root/'project';env=clean_env(root,mode,native,header,args.version)
        (project/'go.mod').write_text(f'module public-proof\n\ngo 1.25.0\n\nrequire {MODULE} v{args.version}\n')
        (project/'main.go').write_text(APP)
        run(['go','mod','tidy'],project,env,root/'resolve.log')
        data=json.loads(run(['go','list','-m','-json',MODULE],project,env,root/'module.json'))
        validate_provenance(data,args.version,root/'gomodcache')
        binary=project/'consumer'
        run(['go','build','-o',str(binary),'.'],project,env,root/'build.log')
        receipt=json.loads(run([str(binary)],project,env,root/'runtime.json'))
        if receipt.get('result')!='PASS': raise ValueError('runtime success receipt missing')
        receipts.append({'mode':mode,'module':data,'runtime':receipt})
    report={'result':'PASS','public_registry_proof':True,'version':args.version,'native_archive_sha256':sha(args.native_archive),'native_sha256':sha(native),'header_sha256':sha(header),'modes':receipts}
    (output/'result.json').write_text(json.dumps(report,indent=2)+'\n')
    print(f'PUBLIC_GO_CONSUMER_PASS report_dir={output}')

if __name__=='__main__': main()

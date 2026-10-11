"""Adversarial provenance and isolation checks for public Go verification."""
import importlib.util
import os
from pathlib import Path
import tempfile
import tarfile
import io
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('public_go',Path(__file__).with_name('public_go.py'))
proof=importlib.util.module_from_spec(spec);spec.loader.exec_module(proof)

class PublicGoTest(unittest.TestCase):
    def test_private_environment_cannot_change_resolution_or_supply_credentials(self):
        with patch.dict(os.environ,{'GOPROXY':'file:///private','GOPRIVATE':'*','GOSUMDB':'off','GOFLAGS':'-modfile=/private','GIT_CONFIG_COUNT':'2','GIT_CONFIG_KEY_1':'url.file:///secret.insteadOf','GIT_CONFIG_VALUE_1':'https://','GITHUB_TOKEN':'secret','SSH_AUTH_SOCK':'secret','DYLD_LIBRARY_PATH':'/stale','CGO_LDFLAGS':'/stale'}):
            proxy=proof.clean_env(Path('/proof/proxy'),'proxy',Path('/native/lib.dylib'),Path('/native/aster_ffi.h'),'0.3.1')
            direct=proof.clean_env(Path('/proof/direct'),'direct',Path('/native/lib.dylib'),Path('/native/aster_ffi.h'),'0.3.1')
        self.assertEqual(proxy['GOPROXY'],'https://proxy.golang.org')
        self.assertEqual(direct['GOPROXY'],'direct')
        self.assertEqual(proxy['GOSUMDB'],'sum.golang.org')
        self.assertEqual(proxy['GOPRIVATE'],'')
        self.assertEqual(proxy['GIT_CONFIG_COUNT'],'1')
        for key in ('GITHUB_TOKEN','SSH_AUTH_SOCK','GOFLAGS','GIT_CONFIG_KEY_1','DYLD_LIBRARY_PATH'):
            self.assertNotIn(key,proxy)
        for key in ('HOME','GOPATH','GOMODCACHE','GOCACHE'):
            self.assertNotEqual(proxy[key],direct[key])

    def test_archive_escape_and_symlink_are_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp)
            for kind in ('escape','symlink'):
                archive=root/(kind+'.tar')
                with tarfile.open(archive,'w') as out:
                    item=tarfile.TarInfo('../outside' if kind=='escape' else 'native-link')
                    if kind=='symlink':
                        item.type=tarfile.SYMTYPE;item.linkname='/outside'
                        out.addfile(item)
                    else:
                        item.size=1;out.addfile(item,io.BytesIO(b'x'))
                destination=root/kind;destination.mkdir()
                with self.subTest(kind=kind),self.assertRaises(ValueError):
                    proof.extract(archive,destination)
            self.assertFalse((root/'outside').exists())

    def test_wrong_versions_replacements_and_checkout_origins_are_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            cache=Path(tmp)/'cache';directory=cache/'module';directory.mkdir(parents=True)
            good={'Path':proof.MODULE,'Version':'v0.3.1','Dir':str(directory),'Sum':'h1:sum','GoModSum':'h1:mod'}
            proof.validate_provenance(good,'0.3.1',cache)
            bad=[{'Version':'v0.3.2'},{'Path':'aster-ffi'},{'Replace':{}},{'Dir':tmp},{'Sum':''},{'GoModSum':''}]
            for mutation in bad:
                with self.subTest(mutation=mutation),self.assertRaises(ValueError):
                    proof.validate_provenance(dict(good,**mutation),'0.3.1',cache)

if __name__=='__main__': unittest.main()

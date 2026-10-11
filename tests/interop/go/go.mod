module aster-interop-echo

go 1.25.0

require sdk.getaster.now/go v0.0.0

require (
	github.com/BurntSushi/toml v1.6.0 // indirect
	github.com/apache/fory/go/fory v1.7.6 // indirect
)

replace sdk.getaster.now/go => ../../../bindings/go

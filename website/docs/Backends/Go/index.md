# Go

## Config

Add the following to `.hephconfig`:

```yaml title=".hephconfig"
build_files:
  roots:
    go_backend:
      # It is best practise to pin a specific commit instead of master
      uri: git://github.com/hephbuild/heph.git@master:/backend/go

go_backend:
  version: 1.18.4
  go: //go_backend:go|go
  gofmt: //go_backend:go|gofmt
```

This will import the go backend and configure it. 

### `go_mod`

Place this rule next to your `go.mod`. This will trigger static code analysis and generate the corresponding tree of targets needed for building * testing your module.

You can specify parameters for the generated targets:

```python
go_mod(cfg={
    'github.com/cool-project/internal/...': {
        'test': {
            'skip': True, # will skip tests in the internal package recursively
        },
    },
    'github.com/cool-project/cmd': {
        'test': {
            'skip': True, # will skip tests in the cmd package
        },
    },
    '...': { # default config for packages
        'test': {
            'pre_run': 'export SOME_KEY=hello1 && ', # will run before invoking the test binary
        },
    },
})
```

### `go_install`

Creates a target that downloads a go binary:

```python
go_install(
    name="protoc-gen-go",
    bin_name="protoc-gen-go",
    pkg="github.com/golang/protobuf/protoc-gen-go",
    version="v1.5.2",
)
```

### `go_bin`

Creates a target that runs the binary (by default in the current package)

```python
go_bin(
	name="run",
	pkg=heph.pkg.addr()+"/some/other/dir", # optionally specify another package
)
```

### `go_bin_build_addr`

Helper function that returns the address of the target that outputs the binary in the defined package:

```python
go_bin_build_addr(heph.pkg.addr())
# //some/dir:go_bin#build
```

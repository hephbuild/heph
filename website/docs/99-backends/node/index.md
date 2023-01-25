# Node

> ⚠️ This is a work in progress

## Config

Add the following to `.hephconfig`:

```yaml title=".hephconfig"
build_files:
  roots:
    node_backend:
      # It is best practise to pin a specific commit instead of master
      uri: git://github.com/hephbuild/heph.git@master:/backend/node

node_backend:
  node: //some/path:node
  yarn: //some/path:yarn
```

And the following in your repo:

```yaml title="some/path/BUILD"
node = node_toolchain(
    name="node",
    version="v16.15.1",
)
yarn_toolchain(
    name="yarn",
    version="v1.22.19",
    node=node,
)
```

This will import the go backend and configure it. 

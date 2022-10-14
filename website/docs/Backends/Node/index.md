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
  version: v16.15.1
  node: //node_backend:node
  yarn: //node_backend:yarn
```

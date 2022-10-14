---
sidebar_position: 3
---
# Usage

The usual way to use heph is to run a single target `heph run //path/to:target` or to run a collection of targets, identified by labels:

```shell
heph query --include test | heph run -
```

This will output all targets with the label `test` and the result will be piped into `heph run`.

See `heph query -h` for available query commands.

A very useful tool is `heph watch`. It works similarly to `heph run` but will continuously watch all input files and will rerun targets that have been affected by file changes. When doing TDD for example it can be used to have a very quick save-test loop:

```shell
heph query //path/to/service/... | heph query --include test - | heph watch -
```

> Note the `-` in `query`, `watch` and `run`: it signifies to read targets from stdin

This will output the targets in the `path/to/service` directory that have the `test` label, and will pipe the result into `heph watch` which will watch for file changes and run the targets. 

> Tip: you can speed up the typing by using the aliases: 
> ```shell
> heph q //path/to/service/... | heph q -i test - | heph w - # heph watch
> heph q -i test - | heph r -                                # heph run
> ```
> And leverage completion: see `heph completion <zsh|fish|bash> --help` for details

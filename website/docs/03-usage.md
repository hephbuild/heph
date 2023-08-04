# Usage

## BUILD files

Targets are declared in BUILD files, see [BUILD](./06-build-file.md) more details 

## Query & Run

The usual way to use heph is to run a single target

```shell
heph run //path/to:target
```

or to run a collection of targets, identified by labels:

```shell
heph query test | heph run -
```

> The first part will output targets with the label `test` and the result will be piped into `heph run` (note the `-`).

You can also compose the query, for example:

```shell
heph query '(//some/pkg/... && test) && !e2e-test' | heph run -
```

See `heph query -h` for available query commands.

When the number of targets becomes too important, it can be pretty hard to remember them all, to make it easy to find them, you can run

```shell
$ heph search pathtarget
//path/to:target
//path/to/some/other:target
```

An interactive mode is also available:

```shell
heph search
```

## Artifacts

When running, output artifacts for each target will be placed somewhere in the `.heph/cache` folder, to get the path of a built artifact, you can request heph to print it out:

```shell
heph run //path/to:target --print-out
```

This will run the target and print the output path to stdout.

## Watch

A very useful tool is `heph watch`. It works similarly to `heph run` but will continuously watch all input files and will rerun targets that have been affected by file changes. When doing TDD for example it can be used to have a very quick save-test loop:

```shell
heph query '//path/to/service/... && test' | heph watch -
```

This will output the targets in the `path/to/service` directory that have the `test` label, and will pipe the result into `heph watch` which will watch for file changes and run the targets. 

> **Tip**: you can speed up the typing by using the aliases: 
> ```shell
> heph q -i test | heph w - # heph watch
> heph q -i test | heph r - # heph run
> ```
> And leverage completion: see `heph completion <zsh|fish|bash> --help` for details

## Debugging

When running target, you may want to peek into what happened before a command is being run, you can use the `--shell` flag, this will prepare the sandbox and open a shell:

```shell
heph run //some:target --shell
```

Two aliases are available:
- `run` to execute the configured commands
- `show` to print the configured command

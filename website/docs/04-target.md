# Target

A target is defined by a name, a set of commands to run, a set of inputs and outputs and environment variables. This execution unit is isolated from the rest of the repo which allows for efficient caching and parallel execution.

```bash title=echo.sh
#!/bin/bash

echo "Hello, world"
```

```python title=BUILD
target(
    name="hello",
    run="./echo.sh",
    deps="echo.sh",
)
```

## Options

| Name             | Type                                           | Default                                           | Description                                                                                  |
|------------------|------------------------------------------------|---------------------------------------------------|----------------------------------------------------------------------------------------------|
| `name`           | `string`                                       | <span class="badge badge--danger">required</span> | Target name                                                                                  |
| `run`            | `string`, `[]string`                           | `[]`                                              | Command(s) to run (see `executor`)                                                           |
| `executor`       | `'bash'`, `'exec'`                             | `'bash'`                                          | See [`executor`](#executor)                                                                  |
| `run_in_cwd`     | `bool`                                         | `False`                                           | Will run the target in the current working directory, use with `sandbox=False`               |
| `pass_args`      | `bool`                                         | `False`                                           | Forward extra args passed to heph to the command (ex: `heph run //some/target -- arg1 arg2`) |
| `cache`          | `bool`, `heph.cache()`                         | `True`                                            | See [`cache`](#cache)                                                                        |
| `support_files`  | `string`, `[]string`                           | `[]`                                              | See [`support_files`](#support_files)                                                        |
| `sandbox`        | `bool`                                         | `True`                                            | Enables sandbox (see [`sandbox`](#sandbox))                                                  |
| `out_in_sandbox` | `bool`                                         | `False`                                           | Will collect output from the sandbox when sandboxing is disabled, use with `sandbox=False`   |
| `gen`            | `bool`                                         | `False`                                           | Marks target as a generating target                                                          |
| `codegen`        | `'link'`, `'copy'`                             | `None`                                            | Enables linking output back into tree, through symlink or hard copy                          |
| `deps`           | `string`, `[]string`, `dict`                   | `[]`                                              | Dependencies required by this target (target and files)                                      |
| `hash_deps`      | `string`, `[]string`, `dict`                   | `deps`                                            | Dependencies used to compute the target hash                                                 |
| `tools`          | `string`, `[]string`, `dict`                   | `[]`                                              | Tools to be exposed to this target (available in `PATH`)                                     |
| `labels`         | `string`, `[]string`                           | `[]`                                              | Labels for this target                                                                       |
| `out`            | `string`, `[]string`, `dict`                   | `[]`                                              | Output files for this target, supports glob                                                  |
| `env`            | `dict`                                         | `{}`                                              | Key/value pairs of environment variables set in the sandbox                                  |
| `pass_env`       | `[]string`                                     | `[]`                                              | Environment variable names to be passed from the outside environment                         |
| `src_env`        | `'ignore'`, `'rel_root'`, `'rel_pkg'`, `'abs'` | `'rel_pkg'`                                       | See [`src_env/out_env`](#src_env-out_env)                                                    |
| `out_env`        | `'ignore'`, `'rel_root'`, `'rel_pkg'`, `'abs'` | `'rel_pkg'`                                       | See [`src_env/out_env`](#src_env-out_env)                                                    |
| `hash_file`      | `'content'`, `'mod_time'`,                     | `'content'`                                       | Method to hash dependencies                                                                  |
| `transitive`     | `heph.target_spec()`                           | `None`                                            | See [`transitive`](#transitive)                                                              |
| `timeout`        | `string`                                       | `None`                                            | Timeout to run target                                                                        |

### `executor`

- `bash`: runs the commands defined in `run` with `bash -c` (each item of the array on a new line)
- `exec` uses the value of `run` as an array of arguments passed to `exec`

### `cache`

- `bool`: enabled or disables cache, will cache the paths defined in `out`
- `heph.cache()`:
```python
heph.cache(
    named: [string], # set named cache to enable
)
```

### `support_files`

Files to be cached, but not be part of `out`, this is useful for support files to be used by a tool during runtime

### `sandbox`

heph will create a directory (where the target cwd will be set), copy the `deps`, override the `PATH` with the needed `tools` and only expose the environment variables defined by `env` and `pass_env`

### `src_env`/`out_env` {#src_env-out_env}

When setting dependencies/output heph will expose those paths as environment variables inside the sandbox:

- For dependencies: `SRC_<NAME>` in case of named dependencies, or just `SRC`
- For output: `OUT_<NAME>` in case of named output, or just `OUT`

By default the value will be the path relative to the package

### `transitive`

You can optionally specify `deps`, `tools`, `env` and `pass_env` that will be transitively applied when the current target is required as `tools` or `deps`:
This is especially useful if a tool requires at runtime another tool to function.

Example:

```python
target(
    name="tool_1",
)

target(
    name="tool_2",
    transitive=heph.target_spec(
        # deps=[],
        tools=[':tool_1'],
        env={'TOOL_1_TOKEN': '1234'},
        # pass_env=[],
    )
)

target(
    name="example",
    tools=[':tool_2'],
)

# This is equivalent to declaring `example` as:

target(
    name="example",
    tools=[':tool_2', ':tool_1'],
    env={'TOOL_1_TOKEN': '1234'},
)


```

## Helper functions

### `text_file`

Creates a target which outputs a text file

```python
text_file(
    name="lorem_txt",
    out="lorem.txt",
    text="lorem ipsum"
)
```

### `json_file`

Creates a target which outputs a json file from a Starlark object

```python
json_file(
    name="lorem_txt",
    out="lorem.txt",
    data={"key": "value"}
)
```

### `tool_target`

Creates a target which will proxy invocation to a binary, this is useful to allow execution of binaries from the cli

```python
tool_target(
    name="go",
    tools=['//thirparty/go:go']
)
```

```bash
heph run //:go -- version
```

### `group`

Creates a target which only collects & outputs a set of files

```python
group(
    name="src_files",
    deps=['file.txt', '//path/to:generate'],
)
```

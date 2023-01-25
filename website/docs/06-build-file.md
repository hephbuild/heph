# BUILD

Targets are defined in `BUILD` (`BUILD.*`) files. `BUILD` files are writted in [Starlark](https://github.com/bazelbuild/starlark) which is a language similar to Python.
Just like Python, you can use control statements to generate targets and functions to abstract things away.

## Functions

### `target`, `text_file`, `json_file`, `tool_target`, `group`

Declares a target, see [Target](./04-target.md).

### `glob`

Returns a list of files matching the pattern. Paths are relative to the current package.

```python
glob('**/*', exclude='some/file.txt')
```

### `to_json`

Returns the string representation of a Starlark object

```python
to_json(['hello']) # => ["hello"]
```

### `fail`

Will stop the execution and exit with an error message

```python
fail("Bad value")
```

### `print`

```python
print("Hello")
```

### `heph.canonicalize`

Returns a canonicalized version of a target

```python
heph.canonicalize(':test') # => //path/to:test
```

### `heph.is_target`

Whether a string is a target

```python
heph.is_target('some string') # => False
heph.is_target(':test') # => True
heph.is_target('//some:test') # => True
```

### `heph.split`

Splits a target address

```python
pkg, target, output = heph.split('//some:addr')
```

### `heph.param`

Gets param (set by `-p`)

```python
value = heph.param('test')
```

### `heph.pkg.dir`

Gets current package dir relative to the repo root

```python
heph.pkg.dir() # some/dir
```

### `heph.pkg.name`

Gets current package name

```python
heph.pkg.name() # dir
```

### `heph.pkg.addr`

Gets current package address

```python
heph.pkg.addr() # //some/dir
```


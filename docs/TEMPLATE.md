# The `template` driver

Render a declared template file with declared variables, into a declared
output. No shell, no subprocess.

```python
target(
    name = "conf",
    driver = "template",
    src = glob("app.conf.j2"),
    out = "app.conf",
    vars = {"host": "0.0.0.0", "port": "8080", "tls": "on"},
)
```

## Why not `sed` or `envsubst`

Filling in a config file is the job those get used for, and both put the
substitution in a shell — where it depends on the host's `sed`, on quoting, and
on whatever else happens to be in scope. `heph tool coreutils` fixes the first
of those (see `COREUTILS.md`); this rule removes the shell from the job
entirely. The template is an input, the rendered file is an output, and the
rendering happens in-process.

## Two properties worth stating

**A template cannot read an undeclared file.** The minijinja environment is
built with no loader, so `{% include %}` and `{% import %}` have nothing to
resolve against and fail rather than reaching into the filesystem. A template
that could read an undeclared file would be a hole in the sandbox, not a
feature.

**An undefined variable is an error that names itself.** Undefined behaviour is
strict, and the set of variables a template references is checked against `vars`
*before* rendering — because minijinja's own message for a missing value is
`undefined value (in template:1)`, which says something is missing without
saying what. Instead:

```
template //pkg:conf: template uses variables that `vars` does not supply:
prot, tls. Supplied: features, host, port
```

A typo produces a fix, not a config file with a hole in it.

The check compares the *root* of a dotted path: minijinja reports
`{{ cfg.port }}` as the undeclared name `cfg.port`, and comparing the whole path
would reject every template that reads a field or calls a method. Loop-bound
names (`{% for item in items %}`) are not reported.

## The cache key

`src` is a hashed input, so editing the template rebuilds the rendered file and
everything downstream of it — that part needs nothing special.

`vars` do not arrive through an input's hashout, so they are folded into the def
hash explicitly. They are held in a `BTreeMap`: `vars` reaches the driver as a
`HashMap`, whose iteration order is randomized per process, and hashing it
unordered would give the same target a different key on every run and never hit
cache.

`TEMPLATE_FORMAT_VERSION` is in the def hash too, for the same reason the exec
driver hashes its own format version: the rendered bytes are a function of the
renderer as well as of the template, so a minijinja upgrade that changes output
must move the key. Bump it when rendering changes.

## Limits

`src` must produce exactly one file. Several have no defensible answer — picking
the first would depend on walk order — so it is refused with the count, and none
names the address that was supposed to produce it.

`vars` values are strings. That is deliberate for now: it keeps the def hash
trivially stable and the rendering total. Structured values (lists, nested
dicts) are the obvious next step if a real template needs them.

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

The renderer lives in `crates/template`, shared with the `tmpl` applet
(`COREUTILS.md`). It is small enough to be tempting to copy and exactly the
wrong thing to copy: the two properties below are *configuration*, and
configuration that exists in two places drifts — a loader enabled on one side
and not the other would be a sandbox hole nobody notices.

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

## Values from another target

A `vars` value may be, or contain, a deferred value — `${read://pkg:name}`, the
contents of another target's single output (see `DEFERRED_VALUES.md`):

```python
target(
    name = "deploy",
    driver = "template",
    src = file("deploy.yaml.j2"),
    out = "deploy.yaml",
    vars = {"image": "${read://infra:registry}/app:${read://app:version}"},
)
```

The host turns each reference into a hashed edge at parse and hands the driver
the bytes at run; the template sees a plain string. The def keeps the reference
as written, so `heph inspect def` and the def hash never build the producer,
while the producer's content reaches the key through the edge.

`${src://…}` is refused in `vars`. It names an artifact's *path in a sandbox*,
and this driver renders in-process with no sandbox to stage one into. Use
`${read://…}` for the contents.

## The cache key

`src` is a hashed input, so editing the template rebuilds the rendered file and
everything downstream of it — that part needs nothing special.

`vars` do not arrive through an input's hashout, so they are folded into the def
hash explicitly — each value's text as written, so a `${read:…}` contributes the
reference and its producer contributes through the edge. They are held in a
`BTreeMap`: the config reaches the driver as a `HashMap`, whose iteration order is
randomized per process, and hashing it unordered would give the same target a
different key on every run and never hit cache.

`TEMPLATE_FORMAT_VERSION` is in the def hash too, for the same reason the exec
driver hashes its own format version: the rendered bytes are a function of the
renderer as well as of the template, so a minijinja upgrade that changes output
must move the key. Bump it when rendering changes — as version 2 did, to stop
minijinja dropping a template's trailing newline. These render *files*, and a
config file that has lost its final newline is wrong.

## Limits

`src` must produce exactly one file. Several have no defensible answer — picking
the first would depend on walk order — so it is refused with the count, and none
names the address that was supposed to produce it.

`vars` values are strings. That is deliberate for now: it keeps the def hash
trivially stable and the rendering total. Structured values (lists, nested
dicts) are the obvious next step if a real template needs them.

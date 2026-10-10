# Plugin functions

A BUILD file calls `heph.<plugin>.<fn>(…)`: `heph.fs.glob("*.go")`,
`heph.go.build_addr("./lib")`, `heph.auth.oidc("github_actions", …)`. Each one is
a **function of a plugin** — a component next to the plugin's provider, drivers,
hooks and runners, not a method on a provider. `<plugin>` is the plugin's name: a
builtin's registration key, or a cdylib's manifest `name`. A plugin can ship
functions and nothing else; the `auth` plugin is a driver plus functions, with no
provider.

`heph inspect functions` lists every function as `<plugin>.<signature>`, sorted
by plugin, then function.

## Writing one

A function implements `PluginFn` (`hplugin::function`, re-exported by the SDK as
`plugin_sdk::function`) and is registered as a `PluginFnDef` — its bare name, its
`FnSignature`, a one-line doc for the LSP, and the handler:

- **Builtin**: return it in `PluginParts.functions` from `register_plugin` /
  `register_plugin_factory`.
- **cdylib**: build `PluginComponents.functions` and `.function_handle` with
  `plugin_sdk::stabby::make_plugin_functions(&cfg.name, defs)` in
  `heph_plugin_create`. Each function's metadata crosses once, at load; metadata
  the host cannot decode, or a function without a signature, fails the plugin's
  load. Every call runs on the plugin's own runtime, so a function may use tokio.

The engine enforces the signature on every call (arity, types, defaults, the
return value). A function returns an `FnOutcome`: the value substituted at the
call site, plus any targets and `provider_state` it **declared**, which the host
merges into the calling package as if the BUILD file had written them.

## A function is pure

This is the author's contract. Nothing checks it at runtime.

A function's outcome — the value **and** what it declares — depends only on:

- its arguments, the calling package (`ctx.pkg`) and the workspace root
  (`ctx.root`);
- the plugin's identity, its YAML `options:`, and what the host handed it at
  construction (`root`, `skip_dirs`, `skip_globs`);
- the host OS and arch;
- for a function documented as tree-reading (`heph.fs.glob`, `heph.fs.parent`),
  the workspace tree under the root, as the plugin's walker sees it.

It must not read the environment, the cwd, `$HOME`, the clock, the network, or a
path outside the root. Its outputs never contain an absolute path: values and
declarations are workspace- or package-relative.

Why: a function's value lands in `TargetSpec.config`, and so in the def hash of
every target that uses it. The host may replay a whole outcome instead of calling
again, within one engine's lifetime (the buildfile provider's package cache) —
never across invocations.

### A bad example

```rust
// DON'T: reads the environment and the clock.
async fn call(&self, ctx: &FnCallContext<'_>, _: FnArgs) -> anyhow::Result<FnOutcome> {
    let who = std::env::var("USER").unwrap_or_default();
    let now = std::time::SystemTime::now();
    Ok(Value::String(format!("//{}:gen_{who}_{now:?}", ctx.pkg)).into())
}
```

Every machine, and every run, computes a different address, so every consumer's
def hash differs per machine and per run: the cache never hits, and within one
engine the value is whatever the first call saw. If a target needs a user name
or a timestamp, it reads it at run time, as a declared input — not at BUILD
evaluation.

## Calling another plugin's function

A plugin reaches another plugin's function through the engine's registry, by
`(plugin, fn)`, with a `FunctionCaller`:

- a builtin gets one in `PluginInit.functions` (a `FunctionSlot`);
- a cdylib gets the registry handle as the second argument of
  `heph_plugin_create` and wraps it with `plugin_sdk::stabby::guest_functions`.

Both are per engine — never stash one in a process-global, or a second engine in
the same process calls into the first one's functions. Both error when called
before the engine has finished registering plugins (the registry is sealed
before the first BUILD evaluation), and after the engine is gone.

Relay what the inner call declared with `FnOutcome::absorb`:

```rust
let mut out = FnOutcome::from(Value::Null());
let addr = out.absorb(functions.call("codegen", "rule", ctx, args).await?);
out.set_value(addr);
```

Reading `.value()` and dropping the rest loses the declarations: a lost target
fails loudly ("target not found"), a lost `provider_state` silently.

## When a name is wrong

A reference to a plugin or function that does not exist fails before the BUILD
file is evaluated, with its file and line:

```
p/BUILD:2:57-70: heph.fss.glob: no plugin named "fss" (did you mean "fs"?); plugins with functions: auth, fs, go. See `heph inspect functions`
p/BUILD:1:8-19: heph.fs.glb: plugin "fs" has no function "glb"; available: base, dir, glob, join, parent. Did you mean "glob"?
```

A cdylib built against another heph ABI is refused at load: "plugin <path> was
built against a different heph ABI (host 0.16.0); reinstall plugins from the same
release as this heph".

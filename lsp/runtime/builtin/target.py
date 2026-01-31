from typing import Union, List, Optional, Literal, Any


def target(
    name: str,
    doc: str = None,
    run: Union[str, List[str]] = [],
    entrypoint: Literal["bash", "sh", "exec"] = "bash",
    run_in_cwd: bool = False,
    pass_args: bool = False,
    cache: Union[bool, Any] = True,
    support_files: Union[str, List[str]] = [],
    sandbox: bool = True,
    out_in_sandbox: bool = False,
    gen: bool = False,
    codegen: Optional[Literal["link", "copy"]] = None,
    deps: Union[str, List[str], dict] = [],
    hash_deps: Union[str, List[str], dict] = [],
    runtime_deps: Union[str, List[str], dict] = [],
    tools: Union[str, List[str], dict] = [],
    labels: Union[str, List[str]] = [],
    out: Union[str, List[str], dict] = [],
    env: dict = {},
    pass_env: List[str] = [],
    src_env: Literal["ignore", "rel_root", "rel_pkg", "abs"] = "rel_pkg",
    out_env: Literal["ignore", "rel_root", "rel_pkg", "abs"] = "rel_pkg",
    hash_file: Literal["content", "mod_time"] = "content",
    transitive: Any = None,
    timeout: str = None,
):
    """Define a target for execution in the heph build system.

    A target is defined by a name, a set of commands to run, a set of inputs and outputs
    and environment variables. This execution unit is isolated from the rest of the repo
    which allows for efficient caching and parallel execution.

    Args:
        name (str): Target name (required)
        doc (str, optional): Documentation for the target
        run (Union[str, List[str]], optional): Command(s) to run (see `executor`)
        entrypoint (Literal['bash', 'sh', 'exec'], optional): How to execute the run commands.
            - 'bash', 'sh': runs commands with `bash -c` or `sh -c` (each array item on new line)
            - 'exec': uses `run` as array of arguments passed to `exec`
        run_in_cwd (bool, optional): Will run the target in the current working directory,
            use with `sandbox=False`
        pass_args (bool, optional): Forward extra args passed to heph to the command
            (ex: `heph run //some/target -- arg1 arg2`)
        cache (Union[bool, Any], optional): Cache configuration.
            - `bool`: enabled or disables cache, will cache the paths defined in `out`
            - `heph.cache()`: named cache configuration
        support_files (Union[str, List[str]], optional): Files to be cached, but not part of `out`.
            Useful for support files used by a tool during runtime
        sandbox (bool, optional): Enables sandbox. heph creates a directory (where target cwd is set),
            copies `deps`, overrides `PATH` with needed `tools`, and only exposes environment
            variables defined by `env` and `pass_env`
        out_in_sandbox (bool, optional): Will collect output from the sandbox when sandboxing
            is disabled, use with `sandbox=False`
        gen (bool, optional): Marks target as a generating target
        codegen (Optional[Literal['link', 'copy']], optional): Enables linking output back into tree,
            through symlink or hard copy
        deps (Union[str, List[str], dict], optional): Dependencies required by this target
            (target and files), will be copied to the sandbox and part of the hash
        hash_deps (Union[str, List[str], dict], optional): Dependencies used only to compute the
            target hash, they will not be copied to the sandbox
        runtime_deps (Union[str, List[str], dict], optional): Dependencies required by this target,
            will be copied to the sandbox, but will not be part of the hash, use with caution
        tools (Union[str, List[str], dict], optional): Tools to be exposed to this target
            (available in `PATH`)
        labels (Union[str, List[str]], optional): Labels for this target
        out (Union[str, List[str], dict], optional): Output files for this target, supports glob
        env (dict, optional): Key/value pairs of environment variables set in the sandbox
        pass_env (List[str], optional): Environment variable names to be passed from the
            outside environment
        src_env (Literal['ignore', 'rel_root', 'rel_pkg', 'abs'], optional): How to expose
            dependency paths as environment variables inside the sandbox.
            For dependencies: `SRC_<NAME>` for named deps, or just `SRC`.
            Default exposes path relative to the package
        out_env (Literal['ignore', 'rel_root', 'rel_pkg', 'abs'], optional): How to expose
            output paths as environment variables inside the sandbox.
            For output: `OUT_<NAME>` for named output, or just `OUT`.
            Default exposes path relative to the package
        hash_file (Literal['content', 'mod_time'], optional): Method to hash dependencies
        transitive (Any, optional): Optional specification of `deps`, `tools`, `env` and `pass_env`
            that will be transitively applied when the current target is required as `tools` or `deps`.
            Useful if a tool requires another tool to function at runtime
        timeout (str, optional): Timeout to run target
    """
    pass


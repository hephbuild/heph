# heph

heph is a build/task execution engine with a provider/driver plugin model. Targets are
isolated, side-effect-free, and content-addressed: declared inputs are hashed before
execution, so identical inputs hit the cache instead of re-running.

Documentation: https://hephbuild.github.io/docs

> ⚠️ **Under heavy development.** Breaking changes may arise at any time.

# Developer setup

Install devenv: https://devenv.sh/getting-started/

## RustRover

Build the devenv

    devenv build

Toolchain location:

    echo "$(pwd)/.devenv/profile/bin"

Standard library:

    devenv shell 'echo $RUST_SRC_PATH'

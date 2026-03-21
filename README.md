# Setup

Install devenv: https://devenv.sh/getting-started/

## RustRover

Build the devenv

    devenv build

Toolchain location:

    echo "$(pwd)/.devenv/profile/bin"

Standard library:

    devenv shell 'echo $RUST_SRC_PATH'

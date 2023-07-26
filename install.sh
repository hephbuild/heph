#!/bin/sh
#
# Downloads a precompiled copy of Heph from our gcp bucket and installs it.
set -e

DEFAULT_URL_BASE="https://storage.googleapis.com/heph-build"

VERSION=`curl -fsSL $DEFAULT_URL_BASE/latest_version`
# Find the os / arch to download. You can do this quite nicely with go env
# but we use this script on machines that don't necessarily have Go itself.
OS=`uname`
sedi='-i'
if [ "$OS" = "Linux" ]; then
    GOOS="linux"
elif [ "$OS" = "Darwin" ]; then
    GOOS="darwin"
    sedi='-i ""'
elif [ "$OS" = "FreeBSD" ]; then
    GOOS="freebsd"
else
    echo "Unknown operating system $OS"
    exit 1
fi

ARCH=`uname -m`
if [ "$ARCH" = "x86_64" ]; then 
    ARCH="amd64"
elif [ "$ARCH" = "aarch64" ]; then
    ARCH="arm64"
elif [ "$ARCH" = "arm64" ]; then
    :
else
    echo "Unsupported cpu arch $ARCH"
    exit 1
fi

HEPH_URL="$DEFAULT_URL_BASE/${VERSION}/heph_${GOOS}_${ARCH}"

LOCATION="${HOME}/.heph"
DIR="${LOCATION}/${VERSION}"
mkdir -p "$DIR"
mkdir -p "${LOCATION}/bin"

if [ "$1" = "--dev" ]; then
    # For local consumption only
    echo "Dev mode"
    cp dev_run.sh /tmp/heph
	  sed $sedi "s|<HEPH_SRC_ROOT>|$(pwd)|g" /tmp/heph
	  mv /tmp/heph "$LOCATION/bin/heph"
elif [ "$1" = "--build" ]; then
    # For local consumption only
    echo "Building..."
    go build -o /tmp/heph github.com/hephbuild/heph/cmd/heph
	  mv /tmp/heph "$LOCATION/bin/heph"
else
    echo "Downloading Heph ${VERSION}..."
    curl -fsSL -o "$DIR/heph" "${HEPH_URL}"
    chmod +x "$DIR/heph"
    # Link it all back up a dir
    for x in `ls "$DIR"`; do
        ln -sf "${DIR}/${x}" "$LOCATION"
    done
    curl $DEFAULT_URL_BASE/hephw -sL --output "${LOCATION}/bin/heph"
    chmod +x "${LOCATION}/bin/heph"
fi

if ! hash heph 2>/dev/null; then
    echo
    if [ -d ~/.local/bin ]; then
        echo "Adding heph to ~/.local/bin..."
        ln -s ~/.heph/heph ~/.local/bin/heph
    elif [ -f ~/.profile ]; then
        echo 'export PATH="${PATH}:${HOME}/.heph/bin"' >> ~/.profile
        echo "Added Heph to path. Run 'source ~/.profile' to pick up the new PATH in this terminal session."
    else
        echo "We were unable to automatically add Heph to the PATH."
        echo "If desired, add this line to your ~/.profile or equivalent:"
        echo "    'PATH=\${PATH}:~/.heph/bin'"
        echo "or install heph system-wide with"
        echo "    'sudo cp ~/.heph/bin/* /usr/local/bin'"
    fi
fi

echo
echo "Heph has been installed under ${LOCATION}"
echo "Run heph --help for more information about how to invoke it."
echo
echo "It is also highly recommended to set up command line completions."
echo "To do so, run the appropriate help command for your shell,"
echo "see heph completion for supported shells, then run "
echo "heph completion <SHELL> -h for instructions"

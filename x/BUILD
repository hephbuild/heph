# A collection of random targets that have certain behavior, for testing

target(
    name = "noop",
    run = "echo ran",
    cache = False,
)

target(
    name = "long-running",
    run = """
for j in {1..100}; do
    echo $j
    sleep 0.1
done
      """,
    cache = False,
)

target(
    name = "trap",
    run = """
trap '' 2
echo "This won't be killed by ctrl-c"
sleep 99999 
      """,
    cache = False,
)

target(
    name = "infinite-running",
    run = """
for j in {1..99999}; do
    echo $j
    sleep 1
done
      """,
    cache = False,
)

waitfail = target(
    name = "wait-fail",
    run = """
    sleep 5
    exit 1
    """,
    cache = False,
)

target(
    name = "dep-wait-fail",
    deps = waitfail,
    run = "echo hello",
    cache = False,
)

target(
    name = "long-log",
    run = [
        "cat /private/var/log/install.log",
        "cat /private/var/log/install.log",
        "cat /private/var/log/install.log",
        "cat /private/var/log/install.log",
    ],
    cache = False,
)

largeout = target(
    name = "large-out",
    run = "dd if=/dev/urandom of=$OUT bs=64M count=64 iflag=fullblock",
    out = "sample.txt",
)

target(
    name = "use-large-out",
    deps = largeout,
    run = "ls -lah $SRC",
    cache = False,
)

target(
    name = "with-task-annotation",
    run = "echo hello",
    cache = False,
    annotations = {"vscode-task": {"file": "x/BUILD"}},
)

target(
    name = "with-launch-annotation",
    run = "echo hello",
    cache = False,
    annotations = {
        "vscode-launch": {
            "file": "x/BUILD",
            "configuration": {
                "type": "node",
                "name": "launch $(addr)",
                "request": "launch",
                "runtimeExecutable": "heph",
                "runtimeArgs": ["run", "$(addr)", "--force"],
                "skipFiles": ["<node_internals>/**"],
                # "remoteRoot": "/ws/.heph/sandbox/mgmt/ndsys/__target_test-http.spec.ts/_dir",
                "localRoot": "${workspaceFolder}",
                "outputCapture": "std",
            },
        },
    },
)

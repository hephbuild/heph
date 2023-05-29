# A collection of random targets that have certain behavior, for testing

target(
    name="noop",
    run="echo ran",
    cache=False,
)

target(
    name="long-running",
    run="""
for j in {1..100}; do
    echo $j
    sleep 0.1
done
      """,
    cache=False,
)

target(
    name="infinite-running",
    run="""
for j in {1..99999}; do
    echo $j
    sleep 1
done
      """,
    cache=False,
)

waitfail=target(
    name="wait-fail",
    run="""
    sleep 5
    exit 1
    """,
    cache=False,
)

target(
    name="dep-wait-fail",
    deps=waitfail,
    run='echo hello',
    cache=False,
)

target(
    name="long-log",
    run=[
        'cat /private/var/log/install.log',
        'cat /private/var/log/install.log',
        'cat /private/var/log/install.log',
        'cat /private/var/log/install.log',
    ],
    cache=False,
)

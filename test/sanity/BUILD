target(
    name="echo",
    run='echo "Hello world"',
    cache=False,
)

target(
    name="count-lorem",
    run='cat $SRC | wc -w | xargs | tee $OUT',
    deps=["lorem.txt"],
    out=['count'],
    cache=False,
)

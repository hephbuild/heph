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

target(
    name="custom-exit-code",
    run='echo "Byebye" && exit 42',
    cache=False,
)

target(
    name="echo-hash-input",
    run='echo $(hash_input)',
    cache=False,
)

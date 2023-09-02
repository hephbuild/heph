largeout=target(
    name="large-out",
    run='dd if=/dev/urandom of=$OUT bs=64M count=16 iflag=fullblock',
    out='sample.txt'
)

target(
    name="use-large-out",
    deps=largeout,
    run='ls -lah $SRC',
    cache=False,
)

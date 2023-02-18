f=text_file(
    name="some_file",
    text="hello",
)

target(
    name='hello',
    deps=f,
    run='cp $SRC $OUT',
    out='out',
)
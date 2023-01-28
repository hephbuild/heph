hello=target(
    name='hello',
    deps={
        'h1': 'hello1.txt',
        'h2': 'hello2.txt',
    },
    run=[
        'TMP='+heph.param('tmp'),
        'touch $TMP/hello',
        'cat $SRC_H1 > $OUT',
    ],
    out='hello.out.txt',
)

hellocp=target(
    name='hellocp',
    deps=hello,
    run='cp $SRC $OUT',
    out='hello.outcp.txt',
)

target(
    name='run',
    deps=hellocp,
    run=[
        'TMP='+heph.param('tmp'),
        'touch $TMP/run',
        'cp $SRC $TMP/hello.txt',
    ],
)

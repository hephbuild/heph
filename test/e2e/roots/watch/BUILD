hello=target(
    name='hello',
    deps='hello.txt',
    run=[
        'cp $SRC $OUT',
    ],
    out='hello.out.txt',
)

target(
    name='run',
    deps=[hello],
    run=[
        'cp $SRC '+heph.param('tmp')+'/hello.txt',
    ],
)

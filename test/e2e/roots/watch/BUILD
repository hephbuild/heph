tmp=heph.param('tmp')
if not tmp:
    fail('tmp is empty')

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
        'cp $SRC {}/hello.txt'.format(tmp),
    ],
)

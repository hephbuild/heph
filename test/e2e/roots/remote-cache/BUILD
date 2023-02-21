tmp=heph.param('tmp')
if not tmp:
    fail('tmp is empty')

target(
    name='with-out',
    run=[
        'TMP='+tmp,
        'touch $TMP/run',
        'echo hello > $OUT',
    ],
    out='out',
)

target(
    name='without-out',
    run=[
        'TMP='+tmp,
        'echo some log',
        'touch $TMP/run',
    ],
)

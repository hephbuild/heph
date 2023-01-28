helloersh=text_file(
    name='helloer_sh',
    text='#!/bin/sh\necho hello'
)

helloer=target(
    name='helloer',
    deps=helloersh,
    run=[
        'chmod +x $SRC',
        'cp $SRC $OUT',
    ],
    out='helloer.sh',
)

target(
    name='run',
    tools=[helloer],
    deps=[helloersh],
    run=[
        '$TOOL_HELLOER_SH >> $OUT',
        'cat $SRC >> $OUT',
    ],
    out='out',
)


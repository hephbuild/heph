t1=target(
    name='t1',
    run='echo hello > $OUT',
    out='t1',
    transitive=heph.target_spec(
        env={
            'TR_T1': '1',
        },
    )
)

t2=target(
    name='t2',
    run='echo hello > $OUT_T2',
    out={'t2': 't2'},
    transitive=heph.target_spec(
        env={
            'TR_T2': '1',
        },
    )
)

g=group(
    name="group",
    deps=t1,
    transitive=heph.target_spec(
        env={
            'TR_GROUP': '1',
        },
        deps=t2,
    )
)

run_sandbox = [
    'echo $TR_GROUP', # group transitive is applied
    'echo $TR_T1', # t1 transitive is applied
    'ls t1',
    'echo $TR_T2', # group & t2 transitive is applied
    'ls t2',
]

target(
    name='test_unamed',
    deps=g,
    run=run_sandbox+[
        'echo $SRC',
        'echo $SRC_T2',
    ],
    cache=False,
)

target(
    name='test_named',
    deps={'g': g},
    run=run_sandbox+[
        'echo $SRC_G',
        'echo $SRC_T2',
    ],
    cache=False,
)

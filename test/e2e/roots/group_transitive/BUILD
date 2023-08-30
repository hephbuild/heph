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

t3=target(
    name='t3',
    run='echo hello > $OUT_T3',
    out={'t3': 't3'},
)

g1=group(
    name="group1",
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
    deps=g1,
    run=run_sandbox+[
        'echo $SRC',
        'echo $SRC_T2',
    ],
    cache=False,
)

target(
    name='test_named',
    deps={'g': g1},
    run=run_sandbox+[
        'echo $SRC_G',
        'echo $SRC_T2',
    ],
    cache=False,
)

def grep_env(match, out="$OUT"):
    return [
        "env > e",
        "(cat e | grep -E '"+match+"' | sort || true) > "+out,
    ]

g2=group(
    name="group2",
    deps=t2,
)

target(
    name='test_unamed-group_named-dep',
    deps=g2+"|t2",
    run=grep_env('SRC'),
    cache=False,
    out="src",
)

g3=group(
    name="group3",
    deps={'t2': t2, 't1': t1},
)

target(
    name='test_named-group_named-dep',
    deps=g3+"|t2",
    run=grep_env('SRC'),
    cache=False,
    out="src",
)

target(
    name='test_named-group_unnamed-dep',
    deps=g3,
    run=grep_env('SRC'),
    cache=False,
    out="src",
)

gd1=group(
    name="group-deep1",
    deps=t1,
    transitive=heph.target_spec(
        deps=t3,
    )
)

gd2=group(
    name="group-deep2",
    deps=[gd1, t2],
)

target(
    name='test_deep-group',
    deps=gd2,
    run=grep_env('SRC'),
    cache=False,
    out="src",
)

target(
    name='test_tool_group1',
    tools=gd1,
    run=grep_env('TOOL_', '$OUT_XTOOL')+grep_env('SRC', '$OUT_XDEPS'),
    cache=False,
    out={
        'xtool': 'tool',
        'xdeps': 'deps',
    },
)

target(
    name='test_tool_group2',
    tools=gd2,
    run=grep_env('TOOL_', '$OUT_XTOOL')+grep_env('SRC', '$OUT_XDEPS'),
    cache=False,
    out={
        'xtool': 'tool',
        'xdeps': 'deps',
    },
)

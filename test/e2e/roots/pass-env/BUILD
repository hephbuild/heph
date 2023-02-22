target(
    name='pass',
    pass_env=['SOME_VAR'],
    run='echo $SOME_VAR > $OUT',
    out='out',
)

target(
    name='runtime_pass',
    runtime_pass_env=['SOME_VAR'],
    run='echo $SOME_VAR > $OUT',
    out='out',
)

trparentpass=target(
    name='tr_parent_pass',
    transitive=heph.target_spec(
        pass_env=['SOME_VAR'],
    ),
)

trparentruntimepass=target(
    name='tr_parent_runtime_pass',
    transitive=heph.target_spec(
        runtime_pass_env=['SOME_VAR'],
    ),
)

target(
    name='tr_pass',
    tools=trparentpass,
    run='echo $SOME_VAR > $OUT',
    out='out',
)

target(
    name='tr_runtime_pass',
    tools=trparentruntimepass,
    run='echo $SOME_VAR > $OUT',
    out='out',
)

load("//test", "e2e_test")

assert_run = 'ls -A1 && echo === && ls -A1 $SANDBOX/../_bin'

target(
    name='tr_base',
    run='touch $OUT',
    out='tr_base',
)

target(
    name='tr_deps_mid',
    run='touch $OUT',
    out='tr_deps_mid',
    transitive=heph.target_spec(
        deps=':tr_base',
        tools=':tr_base',
    ),
)

target(
    name='tr_deps',
    deps=':tr_deps_mid',
    run=assert_run,
    cache=False,
)

e2e_test(
    name="e2e_transitive_deps",
    cmd="heph run //test/features:tr_deps",
    expected_output="""
tr_base
tr_deps_mid
===
tr_base""".strip(),
)

target(
    name='tr_tools_mid',
    transitive=heph.target_spec(
        deps=':tr_base',
        tools=':tr_base',
    ),
)

target(
    name='tr_tools',
    tools=':tr_tools_mid',
    run=assert_run,
    cache=False,
)

e2e_test(
    name="e2e_transitive_tools",
    cmd="heph run //test/features:tr_tools",
    expected_output="""
tr_base
===
tr_base
    """.strip(),
)

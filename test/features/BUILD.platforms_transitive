load("//test", "e2e_test")

sometool1=target(
    name="_sometool1",
    run="touch $OUT",
    out='sometool1',
    transitive=heph.target_spec(
        platforms={
            "name": "docker",
            'options': {
                'image': 'debian:bullseye-slim',
            },
        },
    ),
)

sometool2=target(
    name="_sometool2",
    run="touch $OUT",
    out='sometool2',
    transitive=heph.target_spec(
        platforms={
            "name": "docker",
            'options': {
                'image': 'debian:bullseye-slim',
            },
        }
    ),
)

sometool3=target(
    name="_sometool3",
    run="touch $OUT",
    out='sometool3',
    transitive=heph.target_spec(
        platforms={
            "name": "docker",
            'options': {
                'image': 'debian:trixie-slim',
            },
        }
    ),
)

print_os = [
    'source /etc/os-release',
    'echo $VERSION_CODENAME',
]

target(
    name="plat_tr_ok",
    run=print_os,
    tools=[sometool1, sometool2],
    cache=False,
)

if heph.param('e2e-plat_tr_conflicting'):
    target(
        name="plat_tr_conflicting",
        run=print_os,
        tools=[sometool1, sometool2, sometool3],
        cache=False,
    )

target(
    name="plat_tr_conflicting_ok",
    run=print_os,
    tools=[sometool1, sometool2, sometool3],
    platforms={
        "name": "docker",
        'options': {
            'image': 'debian:bookworm-slim',
        },
    },
    cache=False,
)

e2e_test(
    name="e2e_plat_tr_ok",
    cmd="heph run //test/features:plat_tr_ok",
    expect_output_contains="bullseye",
)

e2e_test(
    name="e2e_plat_tr_conflicting",
    cmd="heph -p e2e-plat_tr_conflicting=1 run //test/features:plat_tr_conflicting",
    expected_failure=True,
)

e2e_test(
    name="e2e_plat_tr_conflicting_ok",
    cmd="heph run //test/features:plat_tr_conflicting_ok",
    expect_output_contains="bookworm",
)

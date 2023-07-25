target(
    name='fail0',
    run='exit 1',
    cache=False,
)

target(
    name='fail1',
    run='sleep 1 && exit 1',
    cache=False,
)

target(
    name='fail2',
    run='sleep 2 && exit 1',
    cache=False,
)

target(
    name='deps',
    deps=['//:fail0', '//:fail1', '//:fail2'],
    run='echo hello',
    cache=False,
)

targets = []
for i in range(0, 999):
    targets.append(target(
        name="echo{}".format(i),
        run="echo {}".format(i),
        cache=False,
    ))

target(
    name='stresstest',
    deps=targets,
    run='echo stresstest',
    cache=False,
)
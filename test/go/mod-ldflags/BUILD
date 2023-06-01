load("//backend/go", "go_mod")
load("//backend/go", "go_bin")

go_mod(cfg={
    '...': {
        'variants': [
            {
                'os': get_os(),
                'arch': get_arch(),
                'name': 'default'
            },
            {
                'os': get_os(),
                'arch': get_arch(),
                'link': {
                    'ldflags': "-X 'main.Version=overriden'",
                },
                'name': 'withflags'
            }
        ],
    }
})

go_bin(
    name="run-default",
    variant_name="default",
)

go_bin(
    name="run-withflags",
    variant_name="withflags",
)

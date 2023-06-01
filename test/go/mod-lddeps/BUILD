load("//backend/go", "go_mod")
load("//backend/go", "go_bin")

version=target(
    name="version",
    out="version",
    run="echo coolversion > $OUT",
)

go_mod(cfg={
    '...': {
        'variants': [
            {
                'os': get_os(),
                'arch': get_arch(),
                'link': {
                    'ldflags': '-X "main.Version=$(cat $SRC_VERSION)"',
                    'deps': {
                        'version': version,
                    },
                },
            }
        ],
    }
})

go_bin(
    name="run",
)

load("//backend/go", "go_mod")

go_mod(cfg={
    'mod-cfg/child1/...': {
        'test': {
            'pre_run': 'export SOME_KEY=hello1 && ',
        },
    },
    'mod-cfg/child2': {
        'test': {
            'pre_run': 'export SOME_KEY=hello2 && ',
        },
    },
    '...': {
        'test': {
            'pre_run': 'export SOME_KEY=hello && ',
        },
    },
})

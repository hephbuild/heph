load("//backend/go", "go_mod")

magic = text_file(
    name = "_magic_value",
    text = "42",
)

go_mod(
    cfg = {
        "mod-cfg/child1/...": {"test": {"run": {"pre_run": "export SOME_KEY=hello1 && "}}},
        "mod-cfg/child2": {"test": {"run": {"pre_run": "export SOME_KEY=hello2 && "}}},
        "mod-cfg/child3": {
            "test": {
                "run": {
                    "cache": False,
                    "deps": {"magic": magic},
                    "pre_run": [
                        "export CHILD3_MAGIC_VALUE2=42",
                        "export CHILD3_MAGIC_VALUE3=$(cat $SRC_MAGIC)",
                    ],
                    "env": {"CHILD3_MAGIC_VALUE1": "42"},
                },
            },
        },
        "...": {"test": {"run": {"pre_run": "export SOME_KEY=hello && "}}},
    },
)

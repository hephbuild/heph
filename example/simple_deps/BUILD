d1 = target(
    name = "d1",
    run = ["env | grep OUT", "echo hello > $OUT"],
    driver = "bash",
    out = "d1"
)

target(
    name = "result",
    run = [
        "env | grep SRC || true",
        "echo $SRC", # will make it fail is its not defined
        "echo $(cat $SRC) > $OUT",
    ],
    driver = "bash",
    deps = {
        "d1": d1,
    },
    out = "result"
)

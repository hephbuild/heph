target(
    name = "t1",
    driver = "bash",
    run = ["echo t1 > $OUT"],
    out = "t1.txt",
)
target(
    name = "t2",
    driver = "bash",
    run = ["echo t2 > $OUT"],
    out = "t2.txt",
)

target(
    name = "g",
    driver = "group",
    deps = ["t1", "t2"],
)

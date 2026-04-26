target(
    name = "hello",
    driver = "bash",
    run = "echo hello world"
)

target(
    name = "hello_out",
    driver = "bash",
    run = "env; echo 'hello world' > $OUT",
    out = "out",
)

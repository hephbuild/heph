load("//backend/go", "go_mod")

go_mod()

# Makes sure this binary get run, building the gen transitive dep
target(
    name="test",
    deps=":go_bin#build",
    run="./$SRC",
    labels=['test'],
)

load("//backend/go", "go_mod")
load("//backend/go", "go_bin_build_addr")

go_mod()

# Makes sure this binary get run, building the gen transitive dep
target(
    name="test",
    deps=go_bin_build_addr(),
    run="./$SRC",
    labels=['test'],
)

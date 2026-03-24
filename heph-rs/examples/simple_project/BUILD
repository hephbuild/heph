# Example BUILD file for a simple Rust project
#
# This demonstrates basic build targets with dependencies.

# Define the package name
package(name = "simple_project")

# Binary target
target(
    name = "main",
    srcs = ["src/main.rs"],
    deps = [
        "//lib:common",
        "//lib:utils",
    ],
    outs = ["main"],
)

# Library target
target(
    name = "helpers",
    srcs = ["src/helpers.rs"],
    deps = ["//lib:common"],
    outs = ["libhelpers.rlib"],
)

# Test target
target(
    name = "test",
    srcs = ["src/main_test.rs"],
    deps = [
        ":main",
        ":helpers",
    ],
    test = True,
)

# Documentation target
target(
    name = "docs",
    srcs = glob(["src/**/*.rs"]),
    outs = ["docs/"],
)

# Library BUILD file
#
# Common libraries used across the project.

package(name = "lib")

# Common utilities library
target(
    name = "common",
    srcs = ["common.rs"],
    outs = ["libcommon.rlib"],
    visibility = ["//..."],  # Visible to all packages
)

# Utils library
target(
    name = "utils",
    srcs = ["utils.rs"],
    deps = [":common"],
    outs = ["libutils.rlib"],
    visibility = ["//..."],
)

# Math library with multiple source files
target(
    name = "math",
    srcs = glob(["math/**/*.rs"]),
    deps = [":common"],
    outs = ["libmath.rlib"],
)

# Integration tests
target(
    name = "integration_test",
    srcs = ["tests/integration.rs"],
    deps = [
        ":common",
        ":utils",
        ":math",
    ],
    test = True,
)

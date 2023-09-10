load("//backend/go", "go_mod")
load("//backend/go", "go_bin_build_addr")

matrix = [("linux", "amd64"), ("darwin", "amd64")]

variants = []
for (os, arch) in matrix:
    variants.append(
        {
            "os": os,
            "arch": arch,
        },
    )

go_mod(
    cfg = {"...": {"variants": variants}},
)

target(
    name = "build_all",
    deps = [
        go_bin_build_addr(
            pkg = heph.pkg.addr(),
            os = "linux",
            arch = "amd64",
        ),
        go_bin_build_addr(
            pkg = heph.pkg.addr(),
            os = "darwin",
            arch = "amd64",
        ),
    ],
)

load("//backend/go", "go_mod")
load("//backend/go", "go_bin")

matrix = [
    ("linux", "amd64"),
    ("linux", "arm64"),
    ("darwin", "amd64"),
    ("darwin", "arm64"),
]

variants = []
for (os, arch) in matrix:
    for tags in [["cooltag"], []]:
        variants.append(
            {
                "os": os,
                "arch": arch,
                "tags": tags,
            },
        )

go_mod(
    cfg = {"...": {"variants": variants}},
)

go_bin(
    name = "run_tag",
    tags = ["cooltag"],
)

go_bin(
    name = "run_notag",
)

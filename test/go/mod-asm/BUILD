load("//backend/go", "go_mod")
load("//backend/go", "go_bin")

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

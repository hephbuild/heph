load("//test", "e2e_test")

modes = [
    (777, "-rwxrwxrwx"),
    (775, "-rwxrwxr-x"),
    (770, "-rwxrwx---"),
    (664, "-rw-rw-r--"),
    (444, "-r--r--r--"),
]

for (mode, expected) in modes:
    smode = str(mode)

    txt = text_file(
        name = "text_file_" + smode,
        text = smode,
        mode = mode,
    )

    t = target(
        name = "_text_file_mode_" + smode,
        deps = txt,
        run = "ls -l $SRC | awk '{print $1;}' | sed 's/@//'",
        cache = False,
        labels = "txt_file_mode",
    )

    e2e_test(
        name = "e2e_text_file_mode_" + smode,
        cmd = "umask 022 && heph run " + t,
        expected_output = expected,
    )

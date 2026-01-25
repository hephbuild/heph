tool = target(
    name = "sometool",
    driver = "bash",
    run = [
        "echo '#!/bin/bash\n echo hello' > $OUT",
        "chmod +x $OUT",
        "mkdir -p support",
        "touch support/file",
    ],
    out = "bin",
    support_files = ["support/"],
)

target(
    name = "use",
    driver = "bash",
    run = [
        "bin",
    ],
    tools = tool,
    cache = False,
)

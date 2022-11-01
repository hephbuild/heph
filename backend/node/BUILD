cfg = CONFIG["node_backend"]

node = cfg["node"]
if not node:
    fail("set node_backend.node")

yarn = cfg["yarn"]
if not yarn:
    fail("set node_backend.yarn")

node_version = cfg["version"]
if not node_version:
    fail("set node_backend.node_version")

target(
    name="node",
    run="./node_install.sh '{}'".format(node_version),
    deps=["node_install.sh", "npm.sh", "npx.sh"],
    out={
        "node": "./node/bin/node",
        "npm": "./npm.sh",
        "npx": "./npx.sh",
    },
    env={
        "OS": get_os(),
        "ARCH": get_arch(),
    },
    cache=["./node", "./npm.sh", "./npx.sh"],
    runtime_env={
        "OUTDIR": "$(outdir)",
    },
)

target(
    name="yarn",
    run="./yarn_install.sh",
    deps=["yarn_install.sh", "yarn.sh"],
    out={
        "yarn": "./yarn.sh",
    },
    cache=["./yarn", "./yarn.sh"],
    runtime_env={
        "OUTDIR": "$(outdir)",
    },
    transitive=heph.target_spec(
        tools=':node',
    )
)

def node_install(name):
    return target(
        name=name,
        run=[
            'echo node: $(node -v)',
            'echo npx: $(npx -v)',
            'echo npm: $(npm -v)',
            'echo yarn: $(yarn -v)',
        ],
        tools=[node, yarn],
        cache=False,
    )

cfg = CONFIG["node_backend"]

node = cfg["node"]
if not node:
    fail("set node_backend.node")

yarn = cfg["yarn"]
if not yarn:
    fail("set node_backend.yarn")

node_toolchain_installsh = group(
    name="_node_toolchain_installsh",
    deps=["node_install.sh"],
)

yarn_toolchain_installsh = group(
    name="_yarn_toolchain_installsh",
    deps=["yarn_install.sh"],
)

npmsh = group(
    name="_npmsh",
    deps=["npm.sh"],
)

npxsh = group(
    name="_npxsh",
    deps=["npx.sh"],
)

yarnsh = group(
    name="_yarnsh",
    deps=["yarn.sh"],
)

def node_toolchain(name, version):
    return target(
        name=name,
        run=[
            "./$SRC_INSTALL '{}'".format(version),
            'mv $SRC_NPM $OUT_NPM',
            'mv $SRC_NPX $OUT_NPX',
        ],
        deps={
            "install": node_toolchain_installsh,
            "npm": npmsh,
            "npx": npxsh,
        },
        out={
            "node": "./node/bin/node",
            "npm": "./npm.sh",
            "npx": "./npx.sh",
        },
        env={
            "OS": get_os(),
            "ARCH": get_arch(),
        },
        support_files=["./node"],
        transitive=heph.target_spec(
            runtime_env={
                "NODE_OUTDIR": "$(outdir)",
            },
        ),
    )

def yarn_toolchain(name, version, node):
    return target(
        name=name,
        run=[
            "./$SRC_INSTALL {}".format(version),
            'mv $SRC_YARN $OUT_YARN',
        ],
        deps={
            'install': yarn_toolchain_installsh,
            'yarn': yarnsh,
        },
        out={
            "yarn": "./yarn.sh",
        },
        support_files=["./yarn"],
        transitive=heph.target_spec(
            tools=node,
            runtime_env={
                "YARN_OUTDIR": "$(outdir)",
            },
        )
    )

def node_install(name):
    return target(
        name=name,
        run=[
            'echo node: $(node -v)',
            'echo npm: $(npm -v)',
            'echo yarn: $(yarn -v)',
        ],
        tools=[node, yarn],
        cache=False,
    )

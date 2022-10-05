cfg = CONFIG["node_backend"]

node = cfg["node"]
if not node:
    fail("set node_backend.node")

yarn = cfg["yarn"]
if not yarn:
    fail("set node_backend.yarn")

tools=[
    ('node', "$TOOL_NODE", [node]),
    ('npm', "$TOOL_NPM", [node]),
    ('npx', "$TOOL_NPX", [node]),
    ('yarn', "$TOOL_YARN", [node, yarn]),
]

for (name, run, tools) in tools:
    tool_target(
        name=name,
        run=run,
        tools=tools,
    )

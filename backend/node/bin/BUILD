cfg = CONFIG["node_backend"]

node = cfg["node"]
if not node:
    fail("set node_backend.node")

yarn = cfg["yarn"]
if not yarn:
    fail("set node_backend.yarn")

tools = [
    ("node", [node + "|node"]),
    ("npm", [node + "|npm"]),
    ("npx", [node + "|npx"]),
    ("yarn", [yarn]),
]

for (name, tools) in tools:
    tool_target(name = name, tools = tools)

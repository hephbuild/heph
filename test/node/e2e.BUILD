load("//test", "e2e_test")

e2e_test(
    name = "sanity_node_version",
    cmd = "heph run //test/node:version",
    expected_output = """
node: v16.15.1
npm: 8.11.0
yarn: 1.22.19""".strip(),
)

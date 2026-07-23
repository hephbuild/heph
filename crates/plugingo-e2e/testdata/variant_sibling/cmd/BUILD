# The `release` variant is declared ONLY here at //cmd. Building
# //cmd:build@v=release threads vp=cmd onto the //lib dependency, whose own
# ancestry does not contain `release` (cmd is a sibling) — so it must resolve
# through the module universe (states_under).
provider_state(
    provider = "go",
    variants = {"release": {"goos": "linux", "goarch": "amd64"}},
)

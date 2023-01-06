target(
    name="version",
    run=[
        'echo node: $(node -v)',
        'echo npx: $(npx -v)',
        'echo npm: $(npm -v)',
        'echo yarn: $(yarn -v)',
    ],
    tools=["//:node", "//:yarn"],
    cache=False,
)

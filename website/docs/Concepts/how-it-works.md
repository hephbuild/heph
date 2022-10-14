---
sidebar_position: 1
---
# How it works

heph works by creating an acyclic graph of dependencies. These are defined in [BUILD](build-file) files.

In order to get reproducible and cacheable units, [targets](target) are used to define the required files, binaries and environment variables available for each run.

Having such precise specifications for each command invocation allows to parallelize runs as much as possible.

In order to have the most efficient runs, you want your targets to be as small as possible, in order to leverage caching as much as possible.

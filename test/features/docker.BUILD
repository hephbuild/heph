load("//test", "e2e_test")

expected = """
PRETTY_NAME="Debian GNU/Linux 11 (bullseye)"
NAME="Debian GNU/Linux"
VERSION_ID="11"
VERSION="11 (bullseye)"
VERSION_CODENAME=bullseye
ID=debian
HOME_URL="https://www.debian.org/"
SUPPORT_URL="https://www.debian.org/support"
BUG_REPORT_URL="https://bugs.debian.org/"
""".strip()

bash = target(
    name = "platform_docker_bash",
    platforms = {
        "name": "docker",
        "options": {"image": "debian:bullseye-slim"},
    },
    run = ["cat /etc/os-release"],
    cache = False,
)

exec = target(
    name = "platform_docker_exec",
    entrypoint = "exec",
    platforms = {
        "name": "docker",
        "options": {"image": "debian:bullseye-slim"},
    },
    run = ["cat", "/etc/os-release"],
    cache = False,
)

e2e_test(
    name = "e2e_platform_docker_bash",
    cmd = "heph r " + bash,
    expected_output = expected,
)

e2e_test(
    name = "e2e_platform_docker_exec",
    cmd = "heph r " + exec,
    expected_output = expected,
)

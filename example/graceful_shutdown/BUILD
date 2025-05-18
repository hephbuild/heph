target(
    name = "print",
    run = [
        """
# trap ctrl-c and call ctrl_c()
trap ctrl_c INT
code=0

function ctrl_c() {
        echo "** Trapped CTRL-C"
        code=1
}

for i in `seq 1 3`; do
    sleep 1
    echo -n "."
done

exit $code
        """
    ],
    driver = "bash",
    cache = False,
)


target(
    name = "shutdown",
    run = [
        """
# trap ctrl-c and call ctrl_c()
trap ctrl_c INT

function ctrl_c() {
        echo "** Trapped CTRL-C"
        echo "exiting..."
        sleep 1
        exit 1
}

for i in `seq 1 3`; do
    sleep 1
    echo -n "."
done
        """
    ],
    driver = "bash",
    cache = False,
)

stuck = target(
    name = "stuck",
    run = [
        """
# trap ctrl-c and call ctrl_c()
trap ctrl_c INT

function ctrl_c() {
        echo "** Trapped CTRL-C"
        sleep 1
        exit 1
}

for i in `seq 1 999`; do
    sleep 1
    echo -n "."
done
        """
    ],
    driver = "bash",
    cache = False,
)

target(
    name = "dep_stuck",
    deps = stuck,
    driver = "bash",
    cache = False,
)

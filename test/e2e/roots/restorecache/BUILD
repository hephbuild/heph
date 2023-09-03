target(
    name='hello',
    deps='file.txt',
    run=[
        'if [ ! -f time.txt ]; then date > time.txt; fi',
        'echo "$(cat $SRC) ran: $(cat time.txt)" > $OUT'
    ],
    out='file.out.txt',
    restore_cache='time.txt',
)

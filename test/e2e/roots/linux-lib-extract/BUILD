extract_libs_sh=text_file(
    name='extract_libs.sh',
    text="""
#!/bin/bash
set -ueo pipefail
PY3=/usr/local/bin/python3
SUP_DIR=./python3_support
SUP_LD=${SUP_DIR}/ld
SUP_LD_LIB=${SUP_DIR}/ldlib
SUP_BIN=${SUP_DIR}/bin
SUP_HOME=${SUP_DIR}/home

heph lle --extra /usr/local/lib/python3.11/lib-dynload $PY3 $SUP_LD_LIB $SUP_LD
cp $PY3 $SUP_BIN

cp $SRC_WRAPSH $OUT

mkdir -p $SUP_HOME/lib/
cp -r /usr/local/lib/python3.11 $SUP_HOME/lib/python3.11
""",
    mode=775,
)

wrapsh=text_file(
    name='wrap.sh',
    text="""
#!/usr/bin/env bash
set -u

export LD_LIBRARY_PATH=$PYTHON3SUP/ldlib
export PYTHONHOME=$PYTHON3SUP/home
exec $PYTHON3SUP/ld $PYTHON3SUP/bin "$@"
""",
    mode=775,
)

py=target(
    name='python3',
    deps={
        'wrapsh': wrapsh,
        'extract_libs': extract_libs_sh,
    },
    run='./extract_libs.sh',
    platforms={
        'os': 'linux',
        'name': 'docker',
        'options': {
            'image': 'python:3',
        },
    },
    tools=['heph', 'docker'],
    src_env='abs',
    support_files='python3_support',
    out='python3',
    transitive=heph.target_spec(
        runtime_env={
            "PYTHON3SUP": "$(outdir)/python3_support",
        },
    ),
)

test_script=text_file(
    name='test_script.py',
    text='print("Hello, world")',
    mode=777,
)

target(
    name='run_py',
    deps={
        'script': test_script,
    },
    tools=py,
    run='python3 $SRC_SCRIPT | tee $OUT',
    entrypoint='sh',
    platforms={
        'os': 'linux',
        'name': 'docker',
        'options': {
            'image': 'alpine',
        },
    },
    out='out',
)

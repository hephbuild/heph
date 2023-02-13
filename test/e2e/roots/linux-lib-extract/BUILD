extract_libs_sh=text_file(
    name='extract_libs.sh',
    text="""
#!/bin/bash
set -ueo pipefail
PY3=/usr/local/bin/python3
heph lle --extra /usr/local/lib/python3.11/lib-dynload $PY3 $OUT_HOME
mkdir -p $OUT_HOME/lib/
cp -r /usr/local/lib/python3.11 $OUT_HOME/lib/python3.11
cp $PY3 $OUT_BIN
""",
    mode=775,
)

py=target(
    name='extract_python_libs',
    deps={
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
    out={
        'bin': 'python3',
        'home': 'pyhome'
    },
    transitive=heph.target_spec(
        runtime_env={
            "PYTHONHOME": "$(outdir)/pyhome",
            # TODO: patchelf
            "LD_LIBRARY_PATH": "$(outdir)/pyhome",
        },
    ),
)

test_script=text_file(
    name='test_script.sh',
    text='print("Hello, world")',
    mode=777,
)

target(
    name='run_py',
    deps={
        'py': py,
        'script': test_script,
    },
    # TODO: patchelf
    run=[
        'LD=$(ls ./pyhome | grep ld-linux)',
        'echo $LD',
        './pyhome/$LD ./python3 $SRC_SCRIPT | tee $OUT',
    ],
    executor='sh',
    platforms={
        'os': 'linux',
        'name': 'docker',
        'options': {
            'image': 'alpine',
        },
    },
    out='out',
    env={
        'SRC_PACK_HOME': '',
    },
)

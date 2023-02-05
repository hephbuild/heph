def docker_run(args="", volumes=[], env=[]):
    volumes+=['$(pwd)']
    env+=['PATH=$(pwd):/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin']
    env=[e if ('=' in e) else e+'=$'+e for e in env]

    volumes_flags = ['--volume={}:{}'.format(p, p) for p in volumes]
    env_flags = ['-e '+e for e in env]

    flags=' '.join(volumes_flags+env_flags)
    flags+=' --workdir=$(pwd)'

    return 'docker run --privileged --cap-add=ALL --rm -t {} {}'.format(flags, args)

create_pack=text_file(
    name='create_pack.sh',
    text="""
#!/bin/bash
heph --log_level=trace lwp /usr/local/bin/python3 --extra /usr/local/lib/python3.11/lib-dynload
mkdir -p ./pyhome/lib/
cp -r /usr/local/lib/python3.11 ./pyhome/lib/python3.11
""",
    mode=777,
)

pack=target(
    name='create_python_pack',
    deps={
        'heph': '//:build_linux_amd64',
        'create_pack': create_pack,
    },
    run=[
        'mv $SRC_HEPH heph',
        docker_run(args="python:3 bash ./create_pack.sh"),
    ],
    tools='docker',
    src_env='abs',
    out={
        'bin': 'python3',
        'home': 'pyhome'
    },
    transitive=heph.target_spec(
        runtime_env={
            "PY_OUTDIR": "$(outdir)",
        },
    ),
)

target(
    name='run_pack',
    deps={
        'pack': pack,
    },
    run=[
        'mv $SRC_LWE lwe',
        docker_run(args='-i alpine lwe ./python3 "$@"', volumes=['$PY_OUTDIR'], env=['PYTHONHOME=$PY_OUTDIR/pyhome']),
    ],
    tools=['docker', 'lwe'],
    cache=False,
)

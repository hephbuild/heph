cfg=CONFIG.get('docker_backend', {})
docker_tools=cfg.get('tools', [])
docker_pass_env=cfg.get('pass_env', [])

docker=target(
    name="docker",
    transitive=heph.target_spec(
        tools=['docker']+docker_tools,
        pass_env=docker_pass_env,
    ),
)

def docker_image(name, dockerfile, deps={}, run=[], build_args=[], platform=None, src_env={}, root=False, **kwargs):
    if type(deps) != "dict":
        deps = {"src": deps}

    args = ' '.join(['--build-arg '+a for a in build_args])
    if platform:
        args += ' --platform='+platform

    build = target(
        name=name,
        run=run+[
            'cd $SANDBOX' if root else None,
            'docker build -f $SRC_DOCKERFILE {} --iidfile $OUT_IID .'.format(args),
            'docker save -o $OUT_TAR $(cat $OUT_IID)',
            'H=$(cat $OUT_IID)',
            'H="${H#sha256:}"',
            'echo $H > $OUT_HASH',
        ],
        tools=docker,
        deps={
                 "dockerfile": dockerfile,
             }|deps,
        out={
            "iid": name+"_iid",
            "tar": name+".tar",
            "hash": name+"_hash",
        },
        src_env={'_default': 'rel_root' if root else 'rel_pkg'}|src_env,
        out_env='rel_root' if root else 'rel_pkg',
        **kwargs,
    )

    imgload = target(
        name=name+"_load",
        run=[
            'docker load -i $SRC_TAR',
        ],
        tools=docker,
        deps={
            'tar': build+"|tar",
            'iid': build+"|iid",
        },
        out=name+'_iid',
        cache=False,
    )

    # target(
    #     name=name+"_run",
    #     run=[
    #         'docker run "$@" $(cat $SRC_IID)',
    #     ],
    #     tools=docker,
    #     deps={
    #         'iid': imgload,
    #     },
    #     pass_args=True,
    #     cache=False,
    #     pass_env='*',
    # )

    return build

def docker_addr(name, repo, image, tag):
    if not heph.is_target(repo):
        repo = text_file(name="_{}_repo".format(name), text=repo)

    if not heph.is_target(image):
        image = text_file(name="_{}_image".format(name), text=image)

    if not heph.is_target(tag):
        tag = text_file(name="_{}_tag".format(name), text=tag)

    return target(
        name=name,
        run='echo "$(cat $SRC_REPO)$(cat $SRC_IMAGE):$(cat $SRC_TAG)" > $OUT',
        out=name,
        deps={
            'repo': repo,
            'image': image,
            'tag': tag,
        }
    )

def docker_push(name, image_build, addr, **kwargs):
    return target(
        name=name,
        run=[
            'ADDR=$(cat $SRC_ADDR)',
            'echo "ADDR: $ADDR"',
            'docker load -i $SRC_TAR',
            'docker tag $(cat $SRC_IMG) $ADDR',
            'docker push $ADDR',
            'echo $ADDR > $OUT'
        ],
        deps={
            'img': image_build+"|iid",
            'tar': image_build+"|tar",
            'addr': addr,
        },
        out='push_addr',
        tools=docker,
        **kwargs,
    )

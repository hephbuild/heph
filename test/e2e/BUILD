for file in glob('roots/**/e2e_main.go'):
    if file.startswith('e2e/lib'):
        continue

    dir=heph.path.dir(file)

    name='e2e_'+dir.replace('roots/', '').replace('/', '_')
    build=target(
        name='_'+name+'_build',
        deps={
            'f': file,
            'files': [
                'go.mod',
            ]+glob(dir+'/**/*')+glob('lib/**/*'),
        },
        tools='//:go|go',
        run=[
            'go build -o $OUT $SRC_F'
        ],
        out='test',
    )

    target(
        name=name,
        deps=glob(dir+'/**/*'),
        run=[
            'cd '+dir,
            '$TOOL_TEST',
        ],
        cache=False,
        tools=[build, 'heph'],
        labels=['e2e_isolated'],
    )

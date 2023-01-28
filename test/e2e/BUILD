main_files=glob('roots/**/e2e_main*.go')

for file in main_files:
    if file.startswith('e2e/lib'):
        continue

    dir=heph.path.dir(file)
    filename=heph.path.base(file)

    name='e2e_'+dir.replace('roots/', '').replace('/', '_')
    if filename != "e2e_main.go":
        name+='_'+filename.replace('e2e_main', '').replace('.go', '').strip('.')

    build=target(
        name='_'+name+'_build',
        deps={
            'f': file,
            'files': [
                file,
                'go.mod',
            ]+glob(dir+'/**/*', exclude=main_files+["**/BUILD"])+glob('lib/**/*'),
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

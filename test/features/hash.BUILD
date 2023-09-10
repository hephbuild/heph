load('//test', 'e2e_test')

no_out=target(
    name='hash_no_out',
    cache=False,
)

unnamed=target(
    name='hash_unnamed_out',
    run='touch $OUT',
    out='out',
    cache=False,
)

named=target(
    name='hash_named_out',
    run=['touch $OUT_NAME1', 'touch $OUT_NAME2'],
    out={'name1': 'out1', 'name2': 'out2'},
    cache=False,
)

expected=[
    ('no_out', no_out, '3cd5202287e0b85ac7f81569fa73acd0', ''),
    ('unnamed', unnamed, '903367ae8f508c49bdee89acecd6beae', ': ced6de92049ed8cc407871c910775ce0'),
    ('named', named, '88fce996e5f00bb3cb042dddfac4c1cb', 'name1: 104f78a8d3577c91355ffb0cb3032aa4\nname2: 0d67e6043ea7a599512bc8eafb59bd87'),
]

for (name, tgt, expected_in, expected_out) in expected:
    hashin=target(
        name='hashin_'+name,
        run='heph query hashin '+tgt,
        tools='heph',
        cache=False,
    )
    hashout=target(
        name='hashout_'+name,
        run='heph query hashout '+tgt,
        tools='heph',
        cache=False,
    )

    e2e_test(
        name='e2e_hashin_'+name,
        cmd='heph run '+hashin,
        expected_output=expected_in,
    )

    e2e_test(
        name='e2e_hashout_'+name,
        cmd='heph run '+hashout,
        expected_output=expected_out,
    )

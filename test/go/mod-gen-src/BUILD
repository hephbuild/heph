load("//backend/go", "go_mod")

hello_src="""
package main

func Hello() string {
	return "hello"
}
"""

hello=text_file(
    name="hello_gen_go",
    out="hello.go",
    text=hello_src,
)

mock_hello_src="""
package main

func Hello() string {
	panic("mock should have been replaced")
}
"""

mock_hello=text_file(
    name="mock_hello_gen_go",
    out="hello.go",
    text=mock_hello_src,
    labels=['go_src']
)

embed_src="""hello"""

text_file(
    name="embed_gen_go",
    out="hello.txt",
    text=embed_src,
    labels=['go_src']
)

go_mod(replace={
    mock_hello: hello,
})

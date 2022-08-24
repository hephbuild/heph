module mod-with-replace-deps

go 1.18

replace github.com/davecgh/go-spew => github.com/lzuwei/go-spew v1.1.2-0.20180921043551-f29cf96d6d71

require github.com/stretchr/testify v1.8.0

require (
	github.com/davecgh/go-spew v1.1.1 // indirect
	github.com/pmezard/go-difflib v1.0.0 // indirect
	gopkg.in/yaml.v3 v3.0.1 // indirect
)

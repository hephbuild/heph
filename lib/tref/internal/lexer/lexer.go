//nolint:govet,forbidigo
package lexer

import "github.com/alecthomas/participle/v2/lexer"

type Ref struct {
	SS    string `@SS`
	Pkg   string `@PackageIdent?`
	Colon string `@Colon`
	Name  string `@NameIdent`
	Args  []Arg  `(At (@@ Comma?)*)?`
}

type RefWithOut struct {
	Ref
	Out     *string `((Pipe|ParamsPipe) @NameIdent)?`
	Filters string  `("filters" OptEq (@OptValue|@OptIdent))?`
}

type Arg struct {
	Key   string   `@Ident`
	Eq    string   `@Eq`
	Value ArgValue `@@?`
}

type ArgValue struct {
	Ident string `  @Ident`
	Str   string `| (StrStart @StrValue StrEnd)`
}

var def = lexer.MustStateful(lexer.Rules{
	"Root": {
		{`SS`, `//`, nil},
		{`Colon`, `:`, lexer.Push("Name")},
		{`PackageIdent`, `[^:]+`, nil},
	},
	"Name": {
		{`Pipe`, `\|`, nil},
		{`At`, `@`, lexer.Push("Params")},
		{`NameIdent`, `[^ @|]+`, nil},
		{"NameEOR", ` `, lexer.Push("Opts")},
	},
	"Params": {
		{"Ident", `[^, =|"]+`, nil},
		{"StrStart", `"`, lexer.Push("ParamString")},
		{"Eq", `=`, nil},
		{"Comma", `,`, nil},
		{"ParamsPipe", `\|`, lexer.Pop()},
		{"ParamsEOR", ` `, lexer.Push("Opts")},
	},
	"ParamString": {
		{"StrValue", `(\\"|[^"])+`, nil},
		{"StrEnd", `"`, lexer.Pop()},
	},
	"Opts": {
		{`OptIdent`, `[^ =]+`, nil},
		{`OptEq`, `=`, nil},
		{"OptValue", `[^ ]+`, nil},
	},
})

func GetLexer() lexer.Definition {
	return def
}

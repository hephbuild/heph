//nolint:govet,forbidigo
package internal

import (
	"fmt"
	"io"
	"strings"

	"github.com/alecthomas/participle/v2"
	"github.com/alecthomas/participle/v2/lexer"
)

type Ref struct {
	SS    string `@SS`
	Pkg   string `@PackageIdent?`
	Colon string `@Colon`
	Name  string `@NameIdent`
	Args  []Arg  `(At (@@ Comma?)*)?`
}

type RefWithOut struct {
	Ref
	Out *string `((Pipe|ParamsPipe) @NameIdent)?`
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
	},
	"Params": {
		{"Ident", `[^, =|"]+`, nil},
		{"StrStart", `"`, lexer.Push("ParamString")},
		{"Eq", `=`, nil},
		{"Comma", `,`, nil},
		{"ParamsPipe", `\|`, lexer.Pop()},
	},
	"ParamString": {
		{"StrValue", `(\\"|[^"])+`, nil},
		{"StrEnd", `"`, lexer.Pop()},
	},
})

var parser = participle.MustBuild[Ref](participle.Lexer(def))
var parserWithOut = participle.MustBuild[RefWithOut](participle.Lexer(def))

func LexDebug(s string, out bool) {
	var parser interface {
		Lex(filename string, r io.Reader) ([]lexer.Token, error)
		Lexer() lexer.Definition
	} = parser
	if out {
		parser = parserWithOut
	}

	toks, err := parser.Lex("", strings.NewReader(s))
	if err != nil {
		fmt.Println(err)

		return
	}

	var buf strings.Builder

	syms := lexer.SymbolsByRune(parser.Lexer())
	for i, tok := range toks {
		if i > 0 {
			buf.WriteString(", ")
		}

		buf.WriteString(fmt.Sprintf("%v{%v}", syms[tok.Type], tok.String()))
	}

	fmt.Println(buf.String())
}

func Parse(s string) (*Ref, error) {
	return parser.ParseString("", s)
}

func ParseWithOut(s string) (*RefWithOut, error) {
	return parserWithOut.ParseString("", s)
}

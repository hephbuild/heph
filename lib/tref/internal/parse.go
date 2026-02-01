package internal

import (
	"fmt"
	"io"
	"strings"

	"github.com/alecthomas/participle/v2"
	"github.com/alecthomas/participle/v2/lexer"
	lexer2 "github.com/hephbuild/heph/lib/tref/internal/lexer"
)

var parser = participle.MustBuild[lexer2.Ref](participle.Lexer(Lexer))
var parserWithOut = participle.MustBuild[lexer2.RefWithOut](participle.Lexer(Lexer), participle.Elide("NameEOR", "ParamsEOR"))

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
		fmt.Println(err) //nolint:forbidigo

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

	fmt.Println(buf.String()) //nolint:forbidigo
}

func IsRelative(s string) bool {
	return strings.HasPrefix(s, ":")
}

func Parse(s, pkg string, optPkg bool) (*lexer2.Ref, error) {
	if optPkg && IsRelative(s) {
		s = "//" + pkg + s
	}

	return parser.ParseString("", s)
}

func ParseWithOut(s string) (*lexer2.RefWithOut, error) {
	return parserWithOut.ParseString("", s)
}

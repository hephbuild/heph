package specs

import (
	"encoding/json"
	"fmt"
	"github.com/hephbuild/heph/utils/xpanic"
	"strings"
)

type Replacer = func(m Matcher) Matcher

type Matcher interface {
	json.Marshaler
	Match(Specer) bool
	String() string
	Intersects(Matcher) IntersectResult
	Simplify() Matcher
	Replace(f Replacer) Matcher
}

type MatcherNot interface {
	Not() Matcher
}

type StringMatcher interface {
	MatchString(s string) bool
}

var PublicMatcher = MustParseMatcher("!:_*")

func isLeaf(a Matcher) bool {
	switch a.(type) {
	case TargetAddr, addrRegexNode, labelNode, labelRegexNode, staticMatcher, funcNode:
		return true
	}

	return false
}

func isLeafGen(a Matcher) bool {
	return IsLabelMatcher(a) || IsAddrMatcher(a)
}

func printToken(t token) string {
	switch t.typ {
	case tokenAddr:
		return fmt.Sprintf("addr: %v", t.value)
	case tokenLabel:
		return fmt.Sprintf("label: %v", t.value)
	default:
		return t.value
	}
}

func parse(tokens []token) Matcher {
	var index int
	ast := parseExpr(tokens, &index)

	if index != len(tokens)-1 {
		panic(fmt.Sprintf("Unexpected token %v", printToken(tokens[index])))
	}

	return ast
}

func parseExpr(tokens []token, index *int) Matcher {
	left := parseTerm(tokens, index)

	for *index < len(tokens) {
		switch tokens[*index].typ {
		case tokenOr:
			*index++
			right := parseTerm(tokens, index)
			left = OrNodeFactory(left, right)
		default:
			return left
		}
	}

	return left
}

func parseTerm(tokens []token, index *int) Matcher {
	left := parseFactor(tokens, index)

	for *index < len(tokens) {
		switch tokens[*index].typ {
		case tokenAnd:
			*index++
			right := parseFactor(tokens, index)
			left = AndNodeFactory(left, right)
		default:
			return left
		}
	}

	return left
}

func parseFunctionArg(tokens []token, index *int) AstNode {
	switch tokens[*index].typ {
	case tokenString:
		{
			node := stringNode{value: tokens[*index].value}
			*index++
			return node
		}
	}

	return parseTerm(tokens, index)
}

func parseFactor(tokens []token, index *int) Matcher {
	switch tokens[*index].typ {
	case tokenNot:
		*index++
		expr := parseFactor(tokens, index)
		return NotNodeFactory(expr)
	case tokenLParen:
		*index++
		expr := parseExpr(tokens, index)
		if tokens[*index].typ != tokenRParen {
			panic(fmt.Sprintf("Expected closing parenthesis, got %v", printToken(tokens[*index])))
		}
		*index++
		return expr
	case tokenLabel:
		value := tokens[*index].value
		*index++

		m, err := ParseLabelGlob(value)
		if err != nil {
			panic(err)
		}

		return m
	case tokenAddr:
		value := tokens[*index].value
		*index++

		if strings.HasPrefix(value, ":") {
			value = "//**" + value
		}

		m, err := ParseTargetGlob(value)
		if err != nil {
			panic(err)
		}
		return m
	case tokenFunction:
		funcName := tokens[*index].value
		*index++

		matchFunc, ok := matcherFunctions[funcName]
		if !ok {
			panic(fmt.Sprintf("Unknown function %v", funcName))
		}

		args := make([]AstNode, 0)
		for {
			if tokens[*index].typ == tokenRParen {
				*index++
				break
			}

			if len(args) > 0 {
				if tokens[*index].typ != tokenComma {
					panic(fmt.Sprintf("Expected comma, got %v", printToken(tokens[*index])))
				}
				*index++ // eat comma
			}

			expr := parseFunctionArg(tokens, index)
			args = append(args, expr)
		}

		return funcNode{name: funcName, args: args, match: matchFunc}
	default:
		tok := tokens[*index]
		if tok.typ == tokenEOF {
			panic("Unexpected end of expression")
		}

		panic(fmt.Sprintf("Unexpected token: %v", printToken(tok)))
	}
}

func lexAndParse(input string) (_ Matcher, err error) {
	return xpanic.RecoverV(func() (Matcher, error) {
		tokens := lex(input)
		ast := parse(tokens)

		return ast, nil
	})
}

func ParseMatcher(input string) (Matcher, error) {
	return lexAndParse(input)
}

func MustParseMatcher(input string) Matcher {
	m, err := ParseMatcher(input)
	if err != nil {
		panic(err)
	}
	return m
}

func ParseMatcherInPkg(pkg, input string) (Matcher, error) {
	return lexAndParse(input)
}

func MatcherFromIncludeExclude(pkg string, include, exclude []string) (Matcher, error) {
	includeMatchers := make([]Matcher, 0, len(include))
	for _, s := range include {
		m, err := ParseMatcherInPkg(pkg, s)
		if err != nil {
			return nil, err
		}

		includeMatchers = append(includeMatchers, m)
	}
	excludeMatchers := make([]Matcher, 0, len(exclude))
	for _, s := range exclude {
		m, err := ParseMatcherInPkg(pkg, s)
		if err != nil {
			return nil, err
		}

		excludeMatchers = append(excludeMatchers, m)
	}

	if len(includeMatchers) > 0 {
		matcher := OrNodeFactory(includeMatchers...)

		if len(excludeMatchers) > 0 {
			matcher = AndNodeFactory[Matcher](matcher, notNode{OrNodeFactory(excludeMatchers...)})
		}

		return matcher, nil
	} else if len(excludeMatchers) > 0 {
		return notNode{OrNodeFactory(excludeMatchers...)}, nil
	} else {
		return nil, nil
	}
}

func MatcherReplace(m Matcher, f func(m Matcher) Matcher) Matcher {
	return m.Replace(f)
}

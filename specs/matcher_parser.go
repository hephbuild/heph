package specs

import (
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/xpanic"
	"regexp"
	"strings"
)

type Matcher interface {
	Match(Specer) bool
	String() string
}

type allMatcher struct{}

var AllMatcher Matcher = allMatcher{}

func (n allMatcher) String() string {
	return "<match all>"
}

func (n allMatcher) Match(t Specer) bool {
	return true
}

type astNode = Matcher

type andNode struct {
	left  astNode
	right astNode
}

func AndNodeFactory(ms ...astNode) Matcher {
	switch len(ms) {
	case 0:
		panic("need at least one node")
	case 1:
		return ms[0]
	}

	n := andNode{left: ms[0], right: ms[1]}
	for i := len(ms); i < 2; i++ {
		n = andNode{left: ms[i], right: n}
	}

	return n
}

func (n andNode) String() string {
	return "(" + n.left.String() + " && " + n.right.String() + ")"
}

func (n andNode) Match(t Specer) bool {
	return n.left.Match(t) && n.right.Match(t)
}

type orNode struct {
	left  astNode
	right astNode
}

func OrNodeFactory(ms ...astNode) Matcher {
	switch len(ms) {
	case 0:
		panic("need at least one node")
	case 1:
		return ms[0]
	}

	return mOrNode[astNode]{nodes: ms}
}

func (n orNode) String() string {
	return "(" + n.left.String() + " || " + n.right.String() + ")"
}

func (n orNode) Match(t Specer) bool {
	return n.left.Match(t) || n.right.Match(t)
}

type mOrNode[T astNode] struct {
	nodes []T
}

func (n mOrNode[T]) String() string {
	ss := ads.Map(n.nodes, func(t T) string {
		return t.String()
	})

	return "(" + strings.Join(ss, " || ") + ")"
}

func (n mOrNode[T]) Match(t Specer) bool {
	for _, node := range n.nodes {
		if node.Match(t) {
			return true
		}
	}

	return false
}

type notNode struct {
	expr astNode
}

func (n notNode) String() string {
	return "!" + n.expr.String()
}

func (n notNode) Match(t Specer) bool {
	return !n.expr.Match(t)
}

type labelNode struct {
	value string
}

func (n labelNode) String() string {
	return n.value
}

func (n labelNode) Match(t Specer) bool {
	return ads.Contains(t.Spec().Labels, n.value)
}

type pkgNode struct {
	pkg string
}

func (n pkgNode) String() string {
	return "//" + n.pkg
}

func (n pkgNode) Match(t Specer) bool {
	tpkg := t.Spec().Package.Path

	if strings.HasSuffix(n.pkg, "...") {
		mpkg := strings.TrimSuffix(n.pkg, "...")
		mpkg = strings.TrimSuffix(mpkg, "/")

		return tpkg == mpkg || strings.HasPrefix(tpkg, mpkg+"/")
	} else if strings.HasSuffix(n.pkg, ".") {
		mpkg := strings.TrimSuffix(n.pkg, ".")
		mpkg = strings.TrimSuffix(mpkg, "/")

		return tpkg == mpkg
	}

	return tpkg == n.pkg
}

type targetNameNode struct {
	regex *regexp.Regexp
}

func (n targetNameNode) String() string {
	return ":" + n.regex.String()
}

func (n targetNameNode) Match(t Specer) bool {
	return n.regex.MatchString(t.Spec().Name)
}

type funcNode struct {
	name string
	args []astNode
}

func (n funcNode) String() string {
	ss := ads.Map(n.args, func(t astNode) string {
		return t.String()
	})

	return n.name + "(" + strings.Join(ss, ", ") + ")"
}

func (n funcNode) Match(t Specer) bool {
	// todo
	return false
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

func parse(tokens []token) astNode {
	var index int
	ast := parseExpr(tokens, &index)

	if index != len(tokens)-1 {
		panic(fmt.Sprintf("Unexpected token %v", printToken(tokens[index])))
	}

	return ast
}

func parseExpr(tokens []token, index *int) astNode {
	left := parseTerm(tokens, index)

	for *index < len(tokens) {
		switch tokens[*index].typ {
		case tokenOr:
			*index++
			right := parseTerm(tokens, index)
			left = orNode{left: left, right: right}
		default:
			return left
		}
	}

	return left
}

func parseTerm(tokens []token, index *int) astNode {
	left := parseFactor(tokens, index)

	for *index < len(tokens) {
		switch tokens[*index].typ {
		case tokenAnd:
			*index++
			right := parseFactor(tokens, index)
			left = andNode{left: left, right: right}
		default:
			return left
		}
	}

	return left
}

func parseFactor(tokens []token, index *int) astNode {
	switch tokens[*index].typ {
	case tokenNot:
		*index++
		expr := parseFactor(tokens, index)
		return notNode{expr: expr}
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
		return labelNode{value: value}

	case tokenAddr:
		value := tokens[*index].value
		*index++

		if strings.HasPrefix(value, ":") {
			r, err := regexp.Compile("^" + strings.ReplaceAll(value[1:], "*", ".*") + "$")
			if err != nil {
				panic(err)
			}

			return targetNameNode{regex: r}
		}

		tp, err := ParseTargetAddrOptional(value)
		if err != nil {
			panic(err)
		}

		if tp.Name == "" {
			return pkgNode{pkg: tp.Package}
		} else {
			return tp
		}
	case tokenFunction:
		value := tokens[*index].value
		*index++

		args := make([]astNode, 0)

		for {
			if tokens[*index].typ == tokenRParen {
				*index++
				return funcNode{name: value, args: args}
			}

			if len(args) > 0 && tokens[*index].typ != tokenComma {
				panic(fmt.Sprintf("Expected comma, got %v", printToken(tokens[*index])))
			}

			expr := parseTerm(tokens, index)
			args = append(args, expr)
		}
	default:
		tok := tokens[*index]
		if tok.typ == tokenEOF {
			panic(fmt.Sprintf("Unexpected end of expression"))
		}

		panic(fmt.Sprintf("Unexpected token: %v", printToken(tok)))
	}
}

func lexAndParse(input string) (_ astNode, err error) {
	return xpanic.RecoverV(func() (astNode, error) {
		tokens := lex(input)
		ast := parse(tokens)

		return ast, nil
	})
}

func ParseMatcher(input string) (Matcher, error) {
	return lexAndParse(input)
}

func ParseMatcherInPkg(pkg, input string) (Matcher, error) {
	return lexAndParse(input)
}

func MatcherFromIncludeExclude(pkg string, include, exclude []string) (Matcher, error) {
	includeMatchers := make([]astNode, 0, len(include))
	for _, s := range include {
		m, err := ParseMatcherInPkg(pkg, s)
		if err != nil {
			return nil, err
		}

		includeMatchers = append(includeMatchers, m)
	}
	excludeMatchers := make([]astNode, 0, len(exclude))
	for _, s := range exclude {
		m, err := ParseMatcherInPkg(pkg, s)
		if err != nil {
			return nil, err
		}

		excludeMatchers = append(excludeMatchers, m)
	}

	matcher := AllMatcher
	if len(includeMatchers) > 0 {
		matcher = OrNodeFactory(includeMatchers...)
	}
	if len(excludeMatchers) > 0 {
		matcher = andNode{matcher, notNode{OrNodeFactory(excludeMatchers...)}}
	}

	return matcher, nil
}

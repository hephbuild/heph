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
	Intersects(Matcher) bool
}

type staticMatcher struct {
	match bool
	str   string
}

var AllMatcher Matcher = staticMatcher{
	match: true,
	str:   "<all>",
}

var NoneMatcher Matcher = staticMatcher{
	match: false,
	str:   "<none>",
}

var PublicMatcher = MustParseMatcher("!:_*")

func (n staticMatcher) String() string {
	return n.str
}

func (n staticMatcher) Match(t Specer) bool {
	return n.match
}

func (n staticMatcher) Intersects(Matcher) bool {
	return n.match
}

type astNode interface {
	String() string
}

type andNode struct {
	left  Matcher
	right Matcher
}

func AndNodeFactory(ms ...Matcher) Matcher {
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

func (n andNode) Intersects(i Matcher) bool {
	if !Intersects(i, n.left) {
		return false
	}

	if !Intersects(i, n.right) {
		return false
	}

	return true
}

func Intersects(a, b Matcher) bool {
	if a == AllMatcher || b == AllMatcher {
		return true
	}

	if a == NoneMatcher || b == NoneMatcher {
		return false
	}

	if a, ok := a.(Matcher); ok {
		if b, ok := b.(Matcher); ok {
			if a.Intersects(b) {
				return true
			}

			if b.Intersects(a) {
				return true
			}
		}
	}

	return false
}

func OrNodeFactory[T Matcher](ms ...T) Matcher {
	switch len(ms) {
	case 0:
		return NoneMatcher
	case 1:
		return ms[0]
	default:
		or := mOrNode{nodes: make([]Matcher, 0, len(ms))}

		for _, m := range ms {
			if any(m) == NoneMatcher {
				continue
			}

			if any(m) == AllMatcher {
				return AllMatcher
			}

			switch n := any(m).(type) {
			case orNode:
				or.nodes = append(or.nodes, n.left, n.right)
			case mOrNode:
				or.nodes = append(or.nodes, n.nodes...)
			default:
				or.nodes = append(or.nodes, m)
			}
		}

		if len(or.nodes) == 2 {
			return orNode{or.nodes[0], or.nodes[1]}
		}

		return or
	}
}

type orNode struct {
	left  Matcher
	right Matcher
}

func (n orNode) String() string {
	return "(" + n.left.String() + " || " + n.right.String() + ")"
}

func (n orNode) Match(t Specer) bool {
	return n.left.Match(t) || n.right.Match(t)
}

func (n orNode) Intersects(i Matcher) bool {
	if Intersects(n.left, i) {
		return true
	}

	if Intersects(n.right, i) {
		return true
	}

	return false
}

type mOrNode struct {
	nodes []Matcher
}

func (n mOrNode) String() string {
	ss := ads.Map(n.nodes, func(t Matcher) string {
		return t.String()
	})

	return "(" + strings.Join(ss, " || ") + ")"
}

func (n mOrNode) Match(t Specer) bool {
	for _, node := range n.nodes {
		if node.Match(t) {
			return true
		}
	}

	return false
}

func (n mOrNode) Intersects(i Matcher) bool {
	for _, node := range n.nodes {
		if Intersects(node, i) {
			return true
		}
	}

	return false
}

type notNode struct {
	expr Matcher
}

func (n notNode) String() string {
	return "!" + n.expr.String()
}

func (n notNode) Match(t Specer) bool {
	return !n.expr.Match(t)
}

func (n notNode) Intersects(i Matcher) bool {
	return !Intersects(n.expr, i)
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

func (n labelNode) Intersects(i Matcher) bool {
	return true
}

type labelRegexNode struct {
	r     *regexp.Regexp
	value string
}

func (n labelRegexNode) String() string {
	return n.value
}

func (n labelRegexNode) Match(t Specer) bool {
	return ads.Some(t.Spec().Labels, func(s string) bool {
		return n.r.MatchString(s)
	})
}

func (n labelRegexNode) Intersects(i Matcher) bool {
	return true
}

type targetRegexNode struct {
	pkg   *regexp.Regexp
	name  *regexp.Regexp
	pkgs  string
	names string
}

func (n targetRegexNode) String() string {
	return "//" + n.pkgs + ":" + n.names
}

func (n targetRegexNode) Match(t Specer) bool {
	return n.MatchPackageName(t.Spec().Package.Path, t.Spec().Name)
}

func (n targetRegexNode) MatchPackageName(pkg, name string) bool {
	if !n.pkg.MatchString(pkg) {
		return false
	}

	return n.name.MatchString(name)
}

func (n targetRegexNode) reducePattern(expr string) string {
	expr = strings.ReplaceAll(expr, "/**/", "/")
	expr = strings.ReplaceAll(expr, "/**", "")
	expr = strings.ReplaceAll(expr, "**/", "")
	expr = strings.ReplaceAll(expr, "**", "")
	expr = strings.ReplaceAll(expr, "*", "")

	return expr
}

func (n targetRegexNode) Intersects(i Matcher) bool {
	if ta, ok := i.(TargetAddr); ok {
		return n.MatchPackageName(ta.Package, ta.Name)
	}

	if ta, ok := i.(targetRegexNode); ok {
		if n.MatchPackageName(n.reducePattern(ta.pkgs), n.reducePattern(ta.names)) {
			return true
		}
	}

	return false
}

type funcNode struct {
	name  string
	args  []astNode
	match func(args []astNode, t Specer) bool
}

func (n funcNode) String() string {
	ss := ads.Map(n.args, func(t astNode) string {
		return t.String()
	})

	return n.name + "(" + strings.Join(ss, ", ") + ")"
}

func (n funcNode) Match(t Specer) bool {
	return n.match(n.args, t)
}

func (n funcNode) Intersects(i Matcher) bool {
	return true
}

var matcherFunctions = map[string]func(args []astNode, t Specer) bool{
	"has_annotation": func(args []astNode, t Specer) bool {
		annotation := args[0].(stringNode).value

		_, ok := t.Spec().Annotations[annotation]
		return ok
	},
}

type stringNode struct {
	value string
}

func (n stringNode) String() string {
	return fmt.Sprintf(`"%v"`, n.value)
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
			left = orNode{left: left, right: right}
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
			left = andNode{left: left, right: right}
		default:
			return left
		}
	}

	return left
}

func parseFunctionArg(tokens []token, index *int) astNode {
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

		args := make([]astNode, 0)
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
			matcher = andNode{matcher, notNode{OrNodeFactory(excludeMatchers...)}}
		}

		return matcher, nil
	} else if len(excludeMatchers) > 0 {
		return notNode{OrNodeFactory(excludeMatchers...)}, nil
	} else {
		return nil, nil
	}
}

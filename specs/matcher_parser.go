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
	Intersects(Matcher) IntersectResult
}

type IntersectResult int

const (
	IntersectTrue    IntersectResult = 1
	IntersectFalse                   = 0
	IntersectUnknown                 = 2
)

func IntersectResultBool(b bool) IntersectResult {
	if b {
		return IntersectTrue
	} else {
		return IntersectFalse
	}
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

func (n staticMatcher) Intersects(Matcher) IntersectResult {
	return IntersectResultBool(n.match)
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

	n := ms[0]
	for i, m := range ms {
		if i == 0 {
			continue
		}
		n = andNode{left: n, right: m}
	}

	return n
}

func (n andNode) String() string {
	return "(" + n.left.String() + " && " + n.right.String() + ")"
}

func (n andNode) Match(t Specer) bool {
	return n.left.Match(t) && n.right.Match(t)
}

func (n andNode) Intersects(i Matcher) IntersectResult {
	return intersectAnd(i, n.left, n.right)
}

func Intersects(a, b Matcher) IntersectResult {
	if a == AllMatcher || b == AllMatcher {
		return IntersectTrue
	}

	if a == NoneMatcher || b == NoneMatcher {
		return IntersectFalse
	}

	r1 := a.Intersects(b)
	if r1 != IntersectUnknown {
		return r1
	}

	r2 := b.Intersects(a)
	if r2 != IntersectUnknown {
		return r2
	}

	return IntersectUnknown
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

func (n orNode) Intersects(i Matcher) IntersectResult {
	return intersectOr(i, n.left, n.right)
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

func intersectOr(i Matcher, ms ...Matcher) IntersectResult {
	hasUnknown := false
	for _, node := range ms {
		r := Intersects(node, i)
		if r == IntersectTrue {
			return IntersectTrue
		}
		if r == IntersectUnknown {
			hasUnknown = true
		}
	}

	if hasUnknown {
		return IntersectUnknown
	}

	return IntersectFalse
}

func intersectAnd(i Matcher, ms ...Matcher) IntersectResult {
	hasUnknown := false
	for _, node := range ms {
		r := Intersects(node, i)
		if r == IntersectFalse {
			return IntersectFalse
		}
		if r == IntersectUnknown {
			hasUnknown = true
		}
	}

	if hasUnknown {
		return IntersectUnknown
	}

	return IntersectTrue
}

func (n mOrNode) Intersects(i Matcher) IntersectResult {
	return intersectOr(i, n.nodes...)
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

func (n notNode) Intersects(i Matcher) IntersectResult {
	r := Intersects(n.expr, i)

	switch r {
	case IntersectTrue:
		return IntersectFalse
	case IntersectFalse:
		return IntersectTrue
	default:
		return IntersectUnknown
	}
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

func (n labelNode) Intersects(i Matcher) IntersectResult {
	switch l := i.(type) {
	case labelNode:
		return IntersectResultBool(n.value == l.value)
	}

	return IntersectUnknown
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

func (n labelRegexNode) Intersects(i Matcher) IntersectResult {
	switch l := i.(type) {
	case labelNode:
		return IntersectResultBool(n.r.MatchString(l.value))
	case labelRegexNode:
		return IntersectResultBool(n.r.MatchString(reducePattern(l.value)))
	}

	return IntersectUnknown
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

func reducePattern(expr string) string {
	expr = strings.ReplaceAll(expr, "/**/", "/")
	expr = strings.ReplaceAll(expr, "/**", "")
	expr = strings.ReplaceAll(expr, "**/", "")
	expr = strings.ReplaceAll(expr, "**", "")
	expr = strings.ReplaceAll(expr, "*", "")

	return expr
}

func (n targetRegexNode) Intersects(i Matcher) IntersectResult {
	switch ta := i.(type) {
	case TargetAddr:
		return IntersectResultBool(n.MatchPackageName(ta.Package, ta.Name))
	case targetRegexNode:
		return IntersectResultBool(
			n.MatchPackageName(reducePattern(ta.pkgs), reducePattern(ta.names)) ||
				ta.MatchPackageName(reducePattern(n.pkgs), reducePattern(n.names)),
		)
	}

	return IntersectUnknown
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

func (n funcNode) Intersects(i Matcher) IntersectResult {
	return IntersectUnknown
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

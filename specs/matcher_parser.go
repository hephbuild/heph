package specs

import (
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/mds"
	"github.com/hephbuild/heph/utils/xpanic"
	"regexp"
	"strings"
)

type Matcher interface {
	Match(Specer) bool
	String() string
	Includes(Matcher) IntersectResult
	Simplify() Matcher
}

type MatcherNot interface {
	Not() Matcher
}

type StringMatcher interface {
	MatchString(s string) bool
}

type IntersectResult int

func (r IntersectResult) Bool() bool {
	if r == IntersectFalse {
		return false
	}

	return true
}

func (r IntersectResult) Not() IntersectResult {
	switch r {
	case IntersectTrue:
		return IntersectFalse
	case IntersectFalse:
		return IntersectTrue
	}

	return IntersectUnknown
}

const (
	IntersectTrue    IntersectResult = 1
	IntersectFalse                   = 0
	IntersectUnknown                 = 2
)

func intersectResultBool(b bool) IntersectResult {
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

func (n staticMatcher) Not() Matcher {
	switch n {
	case AllMatcher:
		return NoneMatcher
	case NoneMatcher:
		return AllMatcher
	default:
		panic("that shouldnt happen")
	}
}

func (n staticMatcher) Simplify() Matcher {
	return n
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

func (n staticMatcher) Includes(Matcher) IntersectResult {
	return intersectResultBool(n.match)
}

type astNode interface {
	String() string
}

type andNode struct {
	left  Matcher
	right Matcher
}

func (n andNode) Not() Matcher {
	return orNode{
		left:  notNode{n.left},
		right: notNode{n.right},
	}
}

func (n andNode) Simplify() Matcher {
	return andNode{left: n.left.Simplify(), right: n.right.Simplify()}
}

func AndNodeFactory[T Matcher](ms ...T) Matcher {
	switch len(ms) {
	case 0:
		return NoneMatcher
	case 1:
		return ms[0]
	default:
		or := mAndNode{nodes: make([]Matcher, 0, len(ms))}

		for _, m := range ms {
			if any(m) == NoneMatcher {
				return NoneMatcher
			}

			if any(m) == AllMatcher {
				continue
			}

			switch n := any(m).(type) {
			case andNode:
				or.nodes = append(or.nodes, n.left, n.right)
			case mAndNode:
				or.nodes = append(or.nodes, n.nodes...)
			default:
				or.nodes = append(or.nodes, m)
			}
		}

		switch len(or.nodes) {
		case 0:
			return NoneMatcher
		case 1:
			return or.nodes[0]
		case 2:
			return andNode{or.nodes[0], or.nodes[1]}
		}

		return or
	}
}

func (n andNode) String() string {
	return "(" + n.left.String() + " && " + n.right.String() + ")"
}

func (n andNode) Match(t Specer) bool {
	return n.left.Match(t) && n.right.Match(t)
}

func (n andNode) Includes(i Matcher) IntersectResult {
	return intersectAnd(i, n.left, n.right)
}

func Intersects(a, b Matcher) IntersectResult {
	if a == AllMatcher || b == AllMatcher {
		return IntersectTrue
	}

	if a == NoneMatcher || b == NoneMatcher {
		return IntersectFalse
	}

	r1 := a.Includes(b)
	if r1 != IntersectUnknown {
		return r1
	}

	r2 := b.Includes(a)
	if r2 != IntersectUnknown {
		return r2
	}

	return IntersectUnknown
}

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

		switch len(or.nodes) {
		case 0:
			return NoneMatcher
		case 1:
			return or.nodes[0]
		case 2:
			return orNode{or.nodes[0], or.nodes[1]}
		}

		return or
	}
}

type orNode struct {
	left  Matcher
	right Matcher
}

func (n orNode) Not() Matcher {
	return andNode{
		left:  notNode{n.left},
		right: notNode{n.right},
	}
}

func (n orNode) Simplify() Matcher {
	return orNode{
		left:  n.left.Simplify(),
		right: n.right.Simplify(),
	}
}

func (n orNode) String() string {
	return "(" + n.left.String() + " || " + n.right.String() + ")"
}

func (n orNode) Match(t Specer) bool {
	return n.left.Match(t) || n.right.Match(t)
}

func (n orNode) Includes(i Matcher) IntersectResult {
	return intersectOr(i, n.left, n.right)
}

type mOrNode struct {
	nodes []Matcher
}

func (n mOrNode) Simplify() Matcher {
	return OrNodeFactory(ads.Map(n.nodes, func(n Matcher) Matcher {
		return n.Simplify()
	})...)
}

func (n mOrNode) Not() Matcher {
	return AndNodeFactory(ads.Map(n.nodes, func(n Matcher) Matcher {
		return notNode{n}
	})...)
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

func (n mOrNode) Includes(i Matcher) IntersectResult {
	return intersectOr(i, n.nodes...)
}

type mAndNode struct {
	nodes []Matcher
}

func (n mAndNode) Simplify() Matcher {
	return AndNodeFactory(ads.Map(n.nodes, func(n Matcher) Matcher {
		return n.Simplify()
	})...)
}

func (n mAndNode) Not() Matcher {
	return OrNodeFactory(ads.Map(n.nodes, func(n Matcher) Matcher {
		return notNode{n}
	})...)
}

func (n mAndNode) String() string {
	ss := ads.Map(n.nodes, func(t Matcher) string {
		return t.String()
	})

	return "(" + strings.Join(ss, " && ") + ")"
}

func (n mAndNode) Match(t Specer) bool {
	for _, node := range n.nodes {
		if !node.Match(t) {
			return false
		}
	}

	return true
}

func (n mAndNode) Includes(i Matcher) IntersectResult {
	return intersectAnd(i, n.nodes...)
}

func intersectOr(i Matcher, ms ...Matcher) IntersectResult {
	hasUnknown := false
	for _, node := range ms {
		r := Intersects(i, node)
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
		r := Intersects(i, node)
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

type notNode struct {
	expr Matcher
}

func (n notNode) Simplify() Matcher {
	var nn Matcher = n
	if nnn, ok := n.expr.(MatcherNot); ok {
		nn = nnn.Not().Simplify()
	}

	for {
		switch nnn := nn.(type) {
		case notNode:
			switch nne := nnn.expr.(type) {
			case notNode:
				nn = nne.expr
				continue
			}
		}

		break
	}

	if nn == n {
		nn = notNode{expr: n.expr.Simplify()}
	} else {
		nn = nn.Simplify()
	}

	return nn
}

func (n notNode) String() string {
	return "!" + n.expr.String()
}

func (n notNode) Match(t Specer) bool {
	return !n.expr.Match(t)
}

func (n notNode) Includes(i Matcher) IntersectResult {
	return IntersectUnknown
}

type labelNode struct {
	value string
	not   bool
}

func (n labelNode) Not() Matcher {
	nr := *(&n)
	nr.not = !nr.not
	return nr
}

func (n labelNode) Simplify() Matcher {
	return n
}

func (n labelNode) String() string {
	if n.not {
		return "!" + n.value
	}
	return n.value
}

func (n labelNode) Match(t Specer) bool {
	return ads.Contains(t.Spec().Labels, n.value) == !n.not
}

func (n labelNode) MatchString(s string) bool {
	return (s == n.value) == !n.not
}

func (n labelNode) Includes(i Matcher) IntersectResult {
	switch l := i.(type) {
	case labelNode:
		if n.not && l.not {
			return IntersectTrue
		}

		r := intersectResultBool(n.value == l.value)

		if n.not || l.not {
			r = r.Not()
		}

		return r
	case labelRegexNode:
		if n.not && l.not {
			return IntersectTrue
		}

		if n.not {
			return IntersectTrue
		}
	}

	return IntersectUnknown
}

type labelRegexNode struct {
	r     *regexp.Regexp
	value string
	not   bool
}

func (n labelRegexNode) Not() Matcher {
	nr := *(&n)
	nr.not = !nr.not
	return nr
}

func (n labelRegexNode) Simplify() Matcher {
	return n
}

func (n labelRegexNode) String() string {
	if n.not {
		return "!" + n.value
	}
	return n.value
}

func (n labelRegexNode) Match(t Specer) bool {
	return ads.Some(t.Spec().Labels, func(s string) bool {
		return n.r.MatchString(s)
	})
}

func (n labelRegexNode) MatchString(s string) bool {
	return n.r.MatchString(s)
}

func (n labelRegexNode) Includes(i Matcher) IntersectResult {
	switch l := i.(type) {
	case labelNode:
		if l.not && !n.not {
			// Force it to run the other way around, to simplify logic
			return IntersectUnknown
		}

		if l.not && n.not {
			return IntersectTrue
		}

		r := intersectResultBool(n.r.MatchString(l.value))
		if n.not || l.not {
			r = r.Not()
		}

		return r
	case labelRegexNode:
		if l.not && n.not {
			return IntersectTrue
		}

		r := intersectResultBool(starIntersect(n.value, l.value, 0, 0))

		if n.not || l.not {
			r = r.Not()
		}

		return r
	}

	return IntersectUnknown
}

type addrRegexNode struct {
	pkg   *regexp.Regexp
	name  *regexp.Regexp
	pkgs  string
	names string
	not   bool
}

func (n addrRegexNode) Simplify() Matcher {
	return n
}

func (n addrRegexNode) Not() Matcher {
	nr := *(&n)
	nr.not = !nr.not
	return nr
}

func (n addrRegexNode) String() string {
	s := "//" + n.pkgs + ":" + n.names
	if n.not {
		s = "!" + s
	}
	return s
}

func (n addrRegexNode) Match(t Specer) bool {
	return n.MatchPackageName(t.Spec().Package.Path, t.Spec().Name)
}

func (n addrRegexNode) MatchPackageName(pkg, name string) bool {
	if !n.pkg.MatchString(pkg) {
		return false
	}

	return n.name.MatchString(name)
}

func matchPrefix(not bool, p, suffix, s, trim string) IntersectResult {
	if p == s {
		if not {
			return IntersectFalse
		}
		return IntersectTrue
	}

	if strings.Count(p, suffix) == 1 && strings.HasSuffix(p, suffix) {
		prefix := strings.TrimSuffix(p, suffix)
		if trim != "" {
			prefix = strings.TrimSuffix(prefix, trim)
		}

		return intersectResultBool(strings.HasPrefix(s, prefix) == !not)
	}

	return IntersectUnknown
}

func matchSuffix(not bool, p, prefix, s, trim string) IntersectResult {
	if p == s {
		if not {
			return IntersectFalse
		}
		return IntersectTrue
	}

	if strings.Count(p, prefix) == 1 && strings.HasPrefix(p, prefix) {
		suffix := strings.TrimPrefix(p, prefix)
		if trim != "" {
			suffix = strings.TrimPrefix(suffix, trim)
		}

		return intersectResultBool(strings.HasSuffix(s, suffix) == !not)
	}

	return IntersectUnknown
}

func matchRegexes(a, b addrRegexNode) IntersectResult {
	not := a.not

	r := matchPrefix(false, a.pkgs, "**", b.pkgs, "/")
	if r == IntersectTrue {
		if not && a.names != "*" {
			return IntersectUnknown
		}
		if r := matchPrefix(not, a.names, "*", b.names, ""); r != IntersectUnknown {
			return r
		}
		if r := matchSuffix(not, a.names, "*", b.names, ""); r != IntersectUnknown {
			return r
		}
	}

	if not {
		r = r.Not()
	}

	return r
}

func (n addrRegexNode) Includes(i Matcher) IntersectResult {
	switch ta := i.(type) {
	case TargetAddr:
		if n.not && ta.not {
			return IntersectTrue
		}

		return intersectResultBool(n.MatchPackageName(ta.Package, ta.Name) == !n.not)
	case addrRegexNode:
		if n.not && ta.not {
			return IntersectTrue
		}

		if n.not || ta.not {
			if n.not {
				r := matchRegexes(n, ta)
				if r == IntersectFalse {
					return IntersectFalse
				}
			}

			if ta.not {
				r := matchRegexes(ta, n)
				if r == IntersectFalse {
					return IntersectFalse
				}
			}

			return IntersectUnknown
		}

		r1 := matchRegexes(n, ta)
		r2 := matchRegexes(ta, n)
		if r1 == IntersectTrue || r2 == IntersectTrue {
			return IntersectTrue
		}
		if r1 == IntersectFalse || r2 == IntersectFalse {
			return IntersectFalse
		}
	}

	return IntersectUnknown
}

type funcNode struct {
	name  string
	args  []astNode
	match func(args []astNode, t Specer) bool
}

func (n funcNode) Simplify() Matcher {
	return n
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

func (n funcNode) Includes(i Matcher) IntersectResult {
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

func IsLabelMatcher(m Matcher) bool {
	if _, ok := m.(labelNode); ok {
		return true
	}

	if _, ok := m.(labelRegexNode); ok {
		return true
	}

	return false
}

func IsAddrMatcher(m Matcher) bool {
	if _, ok := m.(TargetAddr); ok {
		return true
	}

	if _, ok := m.(addrRegexNode); ok {
		return true
	}

	return false
}

type MatcherKind string

const (
	KindAddr  MatcherKind = "addr"
	KindLabel             = "label"
)

type KindMatcher map[MatcherKind]mOrNode

func (a KindMatcher) getKind(m Matcher) MatcherKind {
	switch m.(type) {
	case TargetAddr:
		return KindAddr
	case addrRegexNode:
		return KindAddr
	case labelNode:
		return KindLabel
	case labelRegexNode:
		return KindLabel
	default:
		panic(fmt.Sprintf("unhandled %T", m))
	}
}

func (a KindMatcher) Add(m Matcher) {
	t := a.getKind(m)
	orm := a[t]
	orm.nodes = append(orm.nodes, m)
	a[t] = orm
}

func (a KindMatcher) AllOr() Matcher {
	items := mds.Values(a)
	return OrNodeFactory(items...)
}

func (a KindMatcher) Match(s Specer) bool {
	return a.AllOr().Match(s)
}

func (a KindMatcher) String() string {
	return a.AllOr().String()
}

func (a KindMatcher) Includes(m Matcher) IntersectResult {
	if !isLeafGen(m) {
		return IntersectUnknown
	}

	t := a.getKind(m)
	orm := a[t]

	return orm.Includes(m)
}

func (a KindMatcher) Simplify() Matcher {
	return a
}

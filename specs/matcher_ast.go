package specs

import (
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/mds"
	"regexp"
	"strings"
)

type AstNode interface {
	String() string
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

func (p staticMatcher) Replace(f Replacer) Matcher {
	return f(p)
}

var AllMatcher Matcher = staticMatcher{
	match: true,
	str:   "<all>",
}

var NoneMatcher Matcher = staticMatcher{
	match: false,
	str:   "<none>",
}

func (n staticMatcher) String() string {
	return n.str
}

func (n staticMatcher) Match(t Specer) bool {
	return n.match
}

func (n staticMatcher) Intersects(Matcher) IntersectResult {
	return intersectResultBool(n.match)
}

type orNode struct {
	nodes []Matcher
}

func (n orNode) Replace(f Replacer) Matcher {
	return f(OrNodeFactory(ads.Map(n.nodes, func(n Matcher) Matcher {
		return n.Replace(f)
	})...))
}

func (n orNode) Simplify() Matcher {
	return OrNodeFactory(ads.Map(n.nodes, func(n Matcher) Matcher {
		return n.Simplify()
	})...)
}

func (n orNode) Not() Matcher {
	return AndNodeFactory(ads.Map(n.nodes, func(n Matcher) Matcher {
		return notNode{n}
	})...)
}

func (n orNode) String() string {
	ss := ads.Map(n.nodes, func(t Matcher) string {
		return t.String()
	})

	return "(" + strings.Join(ss, " || ") + ")"
}

func (n orNode) Match(t Specer) bool {
	for _, node := range n.nodes {
		if node.Match(t) {
			return true
		}
	}

	return false
}

func (n orNode) Intersects(i Matcher) IntersectResult {
	return intersectOr(i, n.nodes...)
}

type andNode struct {
	nodes []Matcher
}

func (n andNode) Replace(f Replacer) Matcher {
	return f(AndNodeFactory(ads.Map(n.nodes, func(n Matcher) Matcher {
		return n.Replace(f)
	})...))
}

func (n andNode) Simplify() Matcher {
	return AndNodeFactory(ads.Map(n.nodes, func(n Matcher) Matcher {
		return n.Simplify()
	})...)
}

func (n andNode) Not() Matcher {
	return OrNodeFactory(ads.Map(n.nodes, func(n Matcher) Matcher {
		return notNode{n}
	})...)
}

func (n andNode) String() string {
	ss := ads.Map(n.nodes, func(t Matcher) string {
		return t.String()
	})

	return "(" + strings.Join(ss, " && ") + ")"
}

func (n andNode) Match(t Specer) bool {
	for _, node := range n.nodes {
		if !node.Match(t) {
			return false
		}
	}

	return true
}

func (n andNode) Intersects(i Matcher) IntersectResult {
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

func (p notNode) Replace(f Replacer) Matcher {
	return f(notNode{p.expr.Replace(f)})
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

func (n notNode) Intersects(i Matcher) IntersectResult {
	return IntersectUnknown
}

type labelNode struct {
	value string
	not   bool
}

func (p labelNode) Replace(f Replacer) Matcher {
	return f(p)
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

func (n labelNode) Intersects(i Matcher) IntersectResult {
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

func (p labelRegexNode) Replace(f Replacer) Matcher {
	return f(p)
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

func (n labelRegexNode) Intersects(i Matcher) IntersectResult {
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

func (p addrRegexNode) Replace(f Replacer) Matcher {
	return f(p)
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

	if !strings.Contains(a.pkgs, "**") && !strings.Contains(a.pkgs, "*") {
		return intersectResultBool((a.String() == b.String()) == !not)
	}

	r := matchPrefix(false, a.pkgs, "**", b.pkgs, "/")
	if r == IntersectTrue {
		if not && a.names != "*" {
			return IntersectUnknown
		}
		if b.names == "*" {
			return intersectResultBool((b.names == "*") == !not)
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

func (n addrRegexNode) Intersects(i Matcher) IntersectResult {
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

func IsFuncNode(m Matcher, name string) (bool, []AstNode) {
	if fm, ok := m.(funcNode); ok {
		if fm.name == name {
			return true, fm.args
		}
	}
	return false, nil
}

type funcNode struct {
	name  string
	args  []AstNode
	match func(args []AstNode, t Specer) bool
}

func (p funcNode) Replace(f Replacer) Matcher {
	return f(p)
}

func (n funcNode) Simplify() Matcher {
	return n
}

func (n funcNode) String() string {
	ss := ads.Map(n.args, func(t AstNode) string {
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

var matcherFunctions = map[string]func(args []AstNode, t Specer) bool{
	"has_annotation": func(args []AstNode, t Specer) bool {
		annotation := args[0].(stringNode).value

		_, ok := t.Spec().Annotations[annotation]
		return ok
	},
	"gen_source": func(args []AstNode, t Specer) bool {
		m := args[0].(Matcher)

		return ads.Some(t.Spec().GenSources, func(e string) bool {
			tm, _ := ParseTargetAddr("", e)
			return Intersects(tm, m).Bool()
		})
	},
}

type stringNode struct {
	value string
}

func (n stringNode) String() string {
	return fmt.Sprintf(`"%v"`, n.value)
}

type matcherKind string

const (
	KindAddr  matcherKind = "addr"
	KindLabel             = "label"
)

type KindMatcher map[matcherKind]orNode

func (a KindMatcher) getKind(m Matcher) matcherKind {
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

func (a KindMatcher) allOr() Matcher {
	items := mds.Values(a)
	return OrNodeFactory(items...)
}

func (a KindMatcher) Match(s Specer) bool {
	return a.allOr().Match(s)
}

func (a KindMatcher) String() string {
	return a.allOr().String()
}

func (a KindMatcher) Intersects(m Matcher) IntersectResult {
	if !isLeafGen(m) {
		return IntersectUnknown
	}

	t := a.getKind(m)
	orm := a[t]

	return Intersects(orm, m)
}

func (a KindMatcher) Simplify() Matcher {
	return a
}

func (p KindMatcher) Replace(f Replacer) Matcher {
	return f(KindMatcher(mds.Map(p, func(k matcherKind, v orNode) (matcherKind, orNode) {
		return k, v.Replace(f).(orNode)
	})))
}

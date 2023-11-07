package specs

import (
	"errors"
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"regexp"
	"strings"
)

type TargetAddr struct {
	Package string
	Name    string

	not bool
}

func (p TargetAddr) Simplify() Matcher {
	return p
}

func (p TargetAddr) Match(t Specer) bool {
	return (t.Spec().Addr == p.Full()) == !p.not
}

func (p TargetAddr) String() string {
	if p.not {
		return "!" + p.Full()
	}
	return p.Full()
}

func (n TargetAddr) Not() Matcher {
	nr := *(&n)
	nr.not = !nr.not
	return nr
}

func (p TargetAddr) Intersects(other Matcher) IntersectResult {
	switch ta := other.(type) {
	case TargetAddr:
		if ta.not && p.not {
			return IntersectTrue
		}

		r := intersectResultBool(p.Full() == ta.Full())

		if ta.not || p.not {
			r = r.Not()
		}

		return r
	case addrRegexNode:
		if ta.not && p.not {
			return IntersectTrue
		}
	}

	return IntersectUnknown
}

func (p TargetAddr) Full() string {
	return "//" + p.Package + ":" + p.Name
}

type TargetAddrs []TargetAddr

func (p TargetAddrs) Simplify() Matcher {
	return OrNodeFactory[TargetAddr](p...).Simplify()
}

func (p TargetAddrs) Match(t Specer) bool {
	return OrNodeFactory[TargetAddr](p...).Match(t)
}

func (p TargetAddrs) String() string {
	return OrNodeFactory[TargetAddr](p...).String()
}

func (p TargetAddrs) Intersects(m Matcher) IntersectResult {
	return OrNodeFactory[TargetAddr](p...).Intersects(m)
}

func IsMatcherExplicit(m Matcher) bool {
	if _, ok := m.(TargetAddrs); ok {
		return true
	}

	if _, ok := m.(TargetAddr); ok {
		return true
	}

	return false
}

const letters = `abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ`
const numbers = `0123456789`

var alphanum = letters + numbers

var packageChars = []rune(alphanum + `-._/`)
var packageCharClass = "[a-zA-Z0-9\\-._/]"
var targetNameChars = []rune(alphanum + `-.+_#@=,{}`)
var targetNameCharClass = "[a-zA-Z0-9\\-.+_#@=,{}]"
var outputNameChars = []rune(alphanum + `-_`)

func containsOnly(s string, chars []rune) bool {
	if len(s) == 0 {
		return true
	}

	for _, r := range s {
		if !ads.Contains(chars, r) {
			return false
		}
	}

	return true
}

type errMustMatch struct {
	name  string
	runes []rune
	s     string
}

func (e errMustMatch) Error() string {
	return fmt.Sprintf("%v must match: %v (got %v)", e.name, string(e.runes), e.s)
}

func mustContainOnly(name, s string, rs []rune) error {
	if !containsOnly(s, rs) {
		return errMustMatch{name, rs, s}
	}

	return nil
}

var errTargetNameEmpty = errors.New("target name is empty")

func (p TargetAddr) validate(mode int) error {
	if err := mustContainOnly("package name", p.Package, packageChars); err != nil {
		return err
	}

	if mode&allowName != 0 {
		if mode == allowName && p.Name == "" {
			return errTargetNameEmpty
		}

		if err := mustContainOnly("target name", p.Name, targetNameChars); err != nil {
			return err
		}
	}

	return nil
}

func (p TargetAddr) IsPrivate() bool {
	return strings.HasPrefix(p.Name, "_")
}

func ParseTargetAddr(pkg string, s string) (TargetAddr, error) {
	tp, err := addrParse(pkg, s, allowName)
	if err != nil {
		return TargetAddr{}, err
	}

	err = tp.validate(allowName)
	if err != nil {
		return tp, err
	}

	return tp, nil
}

// star is a placeholder for regex replacement
const star = "<STAR>"

func charsRegex(rs []rune) string {
	return strings.ReplaceAll(string(rs), "-", "\\-")
}

func targetRegexesFactory(tp TargetAddr, packageCharClass, targetNameCharClass string) (*regexp.Regexp, *regexp.Regexp, error) {
	var pkg *regexp.Regexp
	if strings.Contains(tp.Package, "*") {
		if strings.Contains(tp.Package, "***") {
			return nil, nil, fmt.Errorf("unexpected *** in target path")
		}

		charClassSlash := "(?:/|/" + packageCharClass + "+/)"
		charClass := packageCharClass + "*"
		charClassNoSlash := strings.ReplaceAll(charClass, "/", "")

		expr := strings.ReplaceAll(tp.Package, "*", star)
		expr = regexp.QuoteMeta(expr)
		expr = strings.ReplaceAll(expr, "/"+star+star+"/", charClassSlash)
		expr = strings.ReplaceAll(expr, "/"+star+star, charClass)
		expr = strings.ReplaceAll(expr, star+star+"/", charClass)
		expr = strings.ReplaceAll(expr, star+star, charClass)
		expr = strings.ReplaceAll(expr, star, charClassNoSlash)

		r, err := regexp.Compile("^" + expr + "$")
		if err != nil {
			return nil, nil, err
		}
		pkg = r
	} else {
		pkg = regexp.MustCompile("^" + regexp.QuoteMeta(tp.Package) + "$")
	}

	var name *regexp.Regexp
	if strings.Contains(tp.Name, "*") {
		if strings.Contains(tp.Name, "**") {
			return nil, nil, fmt.Errorf("unexpected ** in target name")
		}

		expr := strings.ReplaceAll(tp.Name, "*", star)
		expr = regexp.QuoteMeta(expr)
		expr = strings.ReplaceAll(expr, star, targetNameCharClass+"*")

		r, err := regexp.Compile("^" + expr + "$")
		if err != nil {
			return nil, nil, err
		}
		name = r
	} else {
		name = regexp.MustCompile("^" + regexp.QuoteMeta(tp.Name) + "$")
	}

	return pkg, name, nil
}

func ParseTargetGlob(s string) (Matcher, error) {
	tp, err := addrParse("", s, allowName|allowPkg)
	if err != nil {
		return TargetAddr{}, err
	}

	if strings.HasSuffix(tp.Package, "/...") {
		tp.Package = strings.TrimSuffix(tp.Package, "...") + "**"
	} else if strings.HasSuffix(tp.Package, "/.") {
		tp.Package = strings.TrimSuffix(tp.Package, "/.")
		tp.Name = "*"
	}

	if tp.Name == "" {
		tp.Name = "*"
	}

	if strings.Contains(tp.Name, "*") || strings.Contains(tp.Package, "*") {
		if tp.Name == "*" && (tp.Package == "**" || tp.Package == "**/*") {
			return AllMatcher, nil
		}

		pkg, name, err := targetRegexesFactory(tp, packageCharClass, targetNameCharClass)
		if err != nil {
			return nil, err
		}

		return addrRegexNode{
			pkg:   pkg,
			pkgs:  tp.Package,
			name:  name,
			names: tp.Name,
		}, nil
	}

	err = tp.validate(allowName)
	if err != nil {
		return tp, err
	}

	return tp, nil
}

func ParseLabelGlob(s string) (Matcher, error) {
	if strings.Contains(s, "*") {
		if strings.Contains(s, "**") {
			return nil, fmt.Errorf("unexpected ** in label")
		}

		charClass := "[" + charsRegex(labelChars) + "]*"

		expr := strings.ReplaceAll(s, "*", star)
		expr = regexp.QuoteMeta(expr)
		expr = strings.ReplaceAll(expr, star, charClass)

		r, err := regexp.Compile("^" + expr + "$")
		if err != nil {
			return nil, err
		}

		return labelRegexNode{r: r, value: s}, nil
	}

	return labelNode{value: s}, nil
}

func ParsePkgAddr(s string, validate bool) (string, error) {
	tp, err := addrParse("", s, allowPkg)
	if err != nil {
		return "", err
	}

	if validate {
		err = tp.validate(allowPkg)
		if err != nil {
			return tp.Package, err
		}
	}

	return tp.Package, nil
}

type errInvalidTarget struct {
	s string
}

func (e errInvalidTarget) Error() string {
	return fmt.Sprintf("invalid target: %v", e.s)
}

var errNoNamedOutput = errors.New("cannot reference a named output")
var errGotMultipleColon = errors.New("invalid target, got multiple `:`")
var errRelativeTargetNoPkg = errors.New("relative target provided with no package")
var errTargetAddr = errors.New("found target addr, expected package addr")

const (
	allowName = 1 << iota
	allowPkg
)

func addrParse(pkg string, s string, mode int) (TargetAddr, error) {
	if strings.Contains(s, "|") {
		return TargetAddr{}, errNoNamedOutput
	}

	if strings.HasPrefix(s, "//") {
		s := s[2:]
		if i := strings.Index(s, ":"); i != -1 {
			if strings.Contains(s[i+1:], ":") {
				return TargetAddr{}, errGotMultipleColon
			}

			if mode&allowName == 0 {
				return TargetAddr{}, errTargetAddr
			}

			return TargetAddr{
				Package: s[:i],
				Name:    s[i+1:],
			}, nil
		} else if mode&allowPkg != 0 {
			return TargetAddr{
				Package: s,
			}, nil
		}
	} else if strings.HasPrefix(s, ":") {
		if mode&allowName == 0 {
			return TargetAddr{}, errTargetAddr
		}

		if pkg == "" {
			return TargetAddr{}, errRelativeTargetNoPkg
		}

		return TargetAddr{
			Package: pkg,
			Name:    s[1:],
		}, nil
	}

	return TargetAddr{}, errInvalidTarget{s: s}
}

type TargetOutputPath struct {
	TargetAddr
	Output string
}

func (p TargetOutputPath) Full() string {
	if p.Output != "" {
		return p.TargetAddr.Full() + "|" + p.Output
	}

	return p.TargetAddr.Full()
}

func (p TargetOutputPath) validate() error {
	err := p.TargetAddr.validate(allowName)
	if err != nil {
		return err
	}

	if err := mustContainOnly("output name", p.Output, outputNameChars); err != nil {
		return err
	}

	return nil
}

func TargetOutputParse(pkg string, s string) (TargetOutputPath, error) {
	tp, err := targetOutputParse(pkg, s)
	if err != nil {
		return TargetOutputPath{}, err
	}

	err = tp.validate()
	if err != nil {
		return tp, err
	}

	return tp, err
}

type errInvalidOptions struct {
	parts []string
}

func (e errInvalidOptions) Error() string {
	return fmt.Sprintf("invalid target option, %v", e.parts)
}

var errInvalidOptionsExpectedCloseBracket = errors.New("invalid target options, expected }")

func TargetOutputOptionsParse(pkg string, s string) (TargetOutputPath, map[string]string, error) {
	var options map[string]string
	if strings.HasPrefix(s, "{") {
		i := strings.Index(s, "}")
		if i < 0 {
			return TargetOutputPath{}, nil, errInvalidOptionsExpectedCloseBracket
		}

		ostr := s[1:i]
		if len(ostr) > 0 {
			options = map[string]string{}
			for _, part := range strings.Split(ostr, ",") {
				parts := strings.Split(part, "=")
				if len(parts) != 2 {
					return TargetOutputPath{}, nil, errInvalidOptions{parts}
				}

				options[parts[0]] = parts[1]
			}
		}

		s = s[i+1:]
	}

	tp, err := TargetOutputParse(pkg, s)
	if err != nil {
		return TargetOutputPath{}, nil, err
	}

	return tp, options, nil
}

func targetOutputParse(pkg string, s string) (TargetOutputPath, error) {
	i := strings.Index(s, "|")

	output := ""
	parseStr := s
	if i != -1 {
		parseStr = s[:i]
		output = s[i+1:]
	}

	tp, err := ParseTargetAddr(pkg, parseStr)
	if err != nil {
		return TargetOutputPath{}, err
	}

	return TargetOutputPath{
		TargetAddr: tp,
		Output:     output,
	}, nil
}

var labelChars = []rune(alphanum + `_-`)

func LabelValidate(s string) error {
	if err := mustContainOnly("label", s, labelChars); err != nil {
		return err
	}

	return nil
}

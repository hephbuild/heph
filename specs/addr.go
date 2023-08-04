package specs

import (
	"errors"
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"strings"
)

type TargetAddr struct {
	Package string
	Name    string
}

func (p TargetAddr) Match(t Specer) bool {
	return t.Spec().Addr == p.Full()
}

func (p TargetAddr) String() string {
	return p.Full()
}

func (p TargetAddr) Full() string {
	return "//" + p.Package + ":" + p.Name
}

type TargetAddrs []TargetAddr

func (p TargetAddrs) Match(t Specer) bool {
	return mOrNode[TargetAddr]{nodes: p}.Match(t)
}

func (p TargetAddrs) String() string {
	return mOrNode[TargetAddr]{nodes: p}.String()
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

func IsMatcherTargetAddr(m Matcher) bool {
	_, ok := m.(TargetAddr)
	return ok
}

const letters = `abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ`
const numbers = `0123456789`

var alphanum = letters + numbers

var packageChars = []rune(alphanum + `-._/`)
var targetNameChars = []rune(alphanum + `-.+_#@=,{}`)
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

	if mode&allowTarget != 0 {
		if mode == allowTarget && p.Name == "" {
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
	tp, err := addrParse(pkg, s, allowTarget)
	if err != nil {
		return TargetAddr{}, err
	}

	err = tp.validate(allowTarget)
	if err != nil {
		return tp, err
	}

	return tp, err
}

func ParseTargetAddrOptional(s string) (TargetAddr, error) {
	tp, err := addrParse("", s, allowTarget|allowPkg)
	if err != nil {
		return TargetAddr{}, err
	}

	err = tp.validate(allowTarget | allowPkg)
	if err != nil {
		return tp, err
	}

	return tp, err
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

	return tp.Package, err
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
	allowTarget = 1 << iota
	allowPkg
)

func addrParse(pkg string, s string, mode int) (TargetAddr, error) {
	if strings.Contains(s, "|") {
		return TargetAddr{}, errNoNamedOutput
	}

	if strings.HasPrefix(s, "//") {
		s := s[2:]
		if i := strings.Index(s, ":"); i != -1 {
			if strings.Index(s[i+1:], ":") != -1 {
				return TargetAddr{}, errGotMultipleColon
			}

			if mode&allowTarget == 0 {
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
		if mode&allowTarget == 0 {
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
	err := p.TargetAddr.validate(allowTarget)
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

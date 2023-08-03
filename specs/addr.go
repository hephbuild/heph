package specs

import (
	"errors"
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"path"
	"strings"
)

type TargetPath struct {
	Package string
	Name    string
}

func (p TargetPath) Full() string {
	return "//" + p.Package + ":" + p.Name
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

func (p TargetPath) validate() error {
	if err := mustContainOnly("package name", p.Package, packageChars); err != nil {
		return err
	}

	if err := mustContainOnly("target name", p.Name, targetNameChars); err != nil {
		return err
	}

	return nil
}

func (p TargetPath) IsPrivate() bool {
	return strings.HasPrefix(p.Name, "_")
}

func TargetParse(pkg string, s string) (TargetPath, error) {
	tp, err := targetParse(pkg, s)
	if err != nil {
		return TargetPath{}, err
	}

	err = tp.validate()
	if err != nil {
		return tp, err
	}

	return tp, err
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

func targetParse(pkg string, s string) (TargetPath, error) {
	if strings.Contains(s, "|") {
		return TargetPath{}, errNoNamedOutput
	}

	if strings.HasPrefix(s, "//") {
		s := s[2:]
		i := strings.Index(s, ":")
		if i >= 0 {
			if strings.Index(s[i+1:], ":") != -1 {
				return TargetPath{}, errGotMultipleColon
			}

			return TargetPath{
				Package: s[:i],
				Name:    s[i+1:],
			}, nil
		} else {
			pkg := s

			name := ""
			if pkg != "" {
				name = path.Base(pkg)
			}

			return TargetPath{
				Package: pkg,
				Name:    name,
			}, nil
		}
	} else if strings.HasPrefix(s, ":") {
		if pkg == "" {
			return TargetPath{}, errRelativeTargetNoPkg
		}

		return TargetPath{
			Package: pkg,
			Name:    s[1:],
		}, nil
	}

	return TargetPath{}, errInvalidTarget{s: s}
}

type TargetOutputPath struct {
	TargetPath
	Output string
}

func (p TargetOutputPath) Full() string {
	if p.Output != "" {
		return p.TargetPath.Full() + "|" + p.Output
	}

	return p.TargetPath.Full()
}

func (p TargetOutputPath) validate() error {
	err := p.TargetPath.validate()
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

	tp, err := TargetParse(pkg, parseStr)
	if err != nil {
		return TargetOutputPath{}, err
	}

	return TargetOutputPath{
		TargetPath: tp,
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

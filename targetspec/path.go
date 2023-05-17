package targetspec

import (
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/segmentio/fasthash/fnv1a"
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

var Alphanum = letters + numbers

var packageChars = []rune(Alphanum + `-._/`)
var targetNameChars = []rune(Alphanum + `-.+_#@=,{}`)
var outputNameChars = []rune(Alphanum + `-_`)

func ContainsOnly(s string, chars []rune) bool {
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

var targetPathValidateCache maps.Map[uint32, struct{}]

func (p TargetPath) validate() error {
	k := fnv1a.Init32
	k = fnv1a.AddString32(k, p.Package)
	k = fnv1a.AddString32(k, p.Name)

	if targetPathValidateCache.Has(k) {
		return nil
	}

	if !ContainsOnly(p.Package, packageChars) {
		return fmt.Errorf("package name must match: %v (got %v)", string(packageChars), p.Package)
	}

	if !ContainsOnly(p.Name, targetNameChars) {
		return fmt.Errorf("target name must match: %v (got %v)", string(targetNameChars), p.Name)
	}

	targetPathValidateCache.Set(k, struct{}{})

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

type invalidTargetError struct {
	s string
}

func (e invalidTargetError) Error() string {
	return fmt.Sprintf("invalid target: %v", e.s)
}

func targetParse(pkg string, s string) (TargetPath, error) {
	if strings.Contains(s, "|") {
		return TargetPath{}, fmt.Errorf("cannot reference a named output: %v", s)
	}

	if strings.HasPrefix(s, "//") {
		s := s[2:]
		i := strings.Index(s, ":")
		if i >= 0 {
			if strings.Index(s[i+1:], ":") != -1 {
				return TargetPath{}, fmt.Errorf("invalid target, got multiple `:`")
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
			return TargetPath{}, fmt.Errorf("relative target provided with no package")
		}

		return TargetPath{
			Package: pkg,
			Name:    s[1:],
		}, nil
	}

	return TargetPath{}, invalidTargetError{s: s}
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

var targetOutputPathValidateCache maps.Map[uint32, struct{}]

func (p TargetOutputPath) validate() error {
	err := p.TargetPath.validate()
	if err != nil {
		return err
	}

	k := fnv1a.Init32
	k = fnv1a.AddString32(k, p.Package)
	k = fnv1a.AddString32(k, p.Name)

	if targetOutputPathValidateCache.Has(k) {
		return nil
	}

	if !ContainsOnly(p.Output, outputNameChars) {
		return fmt.Errorf("package name must match: %v (got %v)", string(outputNameChars), p.Output)
	}

	targetOutputPathValidateCache.Set(k, struct{}{})

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

func TargetOutputOptionsParse(pkg string, s string) (TargetOutputPath, map[string]string, error) {
	var options map[string]string
	if strings.HasPrefix(s, "{") {
		i := strings.Index(s, "}")
		if i < 0 {
			return TargetOutputPath{}, nil, fmt.Errorf("invalid target options, expected }")
		}

		ostr := s[1:i]
		if len(ostr) > 0 {
			options = map[string]string{}
			for _, part := range strings.Split(ostr, ",") {
				parts := strings.Split(part, "=")
				if len(parts) != 2 {
					return TargetOutputPath{}, nil, fmt.Errorf("invalid target option, %v", parts)
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

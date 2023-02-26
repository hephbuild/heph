package targetspec

import (
	"bytes"
	"fmt"
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

func (p TargetPath) GetFQN() string {
	return p.Full()
}

const letters = `abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ`
const numbers = `0123456789`

var Alphanum = letters + numbers

var packageRegex = []byte(Alphanum + `-._/`)
var targetNameRegex = []byte(Alphanum + `-.+_#@=,{}`)
var outputNameRegex = []byte(Alphanum + `-_`)

func ContainsOnly(s string, chars []byte) bool {
	if len(s) == 0 {
		return true
	}

	for _, r := range s {
		if !bytes.ContainsRune(chars, r) {
			return false
		}
	}

	return true
}

func (p TargetPath) validate() error {
	if !ContainsOnly(p.Package, packageRegex) {
		return fmt.Errorf("package name must match: %s (got %v)", packageRegex, p.Package)
	}

	if !ContainsOnly(p.Name, targetNameRegex) {
		return fmt.Errorf("target name must match: %s (got %v)", targetNameRegex, p.Name)
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
		if strings.Contains(s, ":") {
			i := strings.Index(s, ":")
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

func (p TargetOutputPath) validate() error {
	err := p.TargetPath.validate()
	if err != nil {
		return err
	}

	if !ContainsOnly(p.Output, outputNameRegex) {
		return fmt.Errorf("package name must match: %s (got %v)", outputNameRegex, p.Output)
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

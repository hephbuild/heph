package utils

import (
	"fmt"
	"path"
	"regexp"
	"strings"
)

type TargetPath struct {
	Package string
	Name    string
}

func (p TargetPath) Full() string {
	return fmt.Sprintf("//%v:%v", p.Package, p.Name)
}

var packageRegex = regexp.MustCompile(`^[A-Za-z\d-_/]*$`)
var targetNameRegex = regexp.MustCompile(`^[A-Za-z\d-_#]*$`)
var outputNameRegex = regexp.MustCompile(`^[A-Za-z\d-_]*$`)

func (p TargetPath) validate() error {
	if !packageRegex.MatchString(p.Package) {
		return fmt.Errorf("package name must match: %v (got %v)", packageRegex.String(), p.Package)
	}

	if !targetNameRegex.MatchString(p.Name) {
		return fmt.Errorf("target name must match: %v (got %v)", targetNameRegex.String(), p.Name)
	}

	return nil
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

func targetParse(pkg string, s string) (TargetPath, error) {
	if strings.Contains(s, "|") {
		return TargetPath{}, fmt.Errorf("cannot reference a named output")
	}

	if strings.HasPrefix(s, "//") {
		if strings.Contains(s, ":") {
			parts := strings.Split(s[2:], ":")
			if len(parts) != 2 {
				return TargetPath{}, fmt.Errorf("invalid target, got multiple `:`")
			}

			return TargetPath{
				Package: parts[0],
				Name:    parts[1],
			}, nil
		} else {
			return TargetPath{
				Package: s[2:],
				Name:    path.Base(s),
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

	return TargetPath{}, fmt.Errorf("invalid target")
}

type TargetOutputPath struct {
	TargetPath
	Output string
}

func (p TargetOutputPath) validate() error {
	err := p.TargetPath.validate()
	if err != nil {
		return err
	}

	if !outputNameRegex.MatchString(p.Output) {
		return fmt.Errorf("package name must match: %v (got %v)", outputNameRegex.String(), p.Output)
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

func targetOutputParse(pkg string, s string) (TargetOutputPath, error) {
	parts := strings.SplitN(s, "|", 2)

	tp, err := TargetParse(pkg, parts[0])
	if err != nil {
		return TargetOutputPath{}, err
	}

	output := ""
	if len(parts) > 1 {
		output = parts[1]
	}

	return TargetOutputPath{
		TargetPath: tp,
		Output:     output,
	}, nil
}

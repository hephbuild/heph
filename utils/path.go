package utils

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
	return fmt.Sprintf("//%v:%v", p.Package, p.Name)
}

const letters = `abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ`
const numbers = `0123456789`
const packageRegex = letters + numbers + `-._/`
const targetNameRegex = letters + numbers + `-.+_#`
const outputNameRegex = letters + numbers + `-_`

func containsOnly(s, chars string) bool {
	if len(s) == 0 {
		return true
	}

	charsb := []byte(chars)
	for _, r := range s {
		if !bytes.ContainsRune(charsb, r) {
			return false
		}
	}

	return true
}

func (p TargetPath) validate() error {
	if !containsOnly(p.Package, packageRegex) {
		return fmt.Errorf("package name must match: %v (got %v)", packageRegex, p.Package)
	}

	if !containsOnly(p.Name, targetNameRegex) {
		return fmt.Errorf("target name must match: %v (got %v)", targetNameRegex, p.Name)
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

	if !containsOnly(p.Output, outputNameRegex) {
		return fmt.Errorf("package name must match: %v (got %v)", outputNameRegex, p.Output)
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

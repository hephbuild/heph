package xstarlark

import (
	"errors"
	"fmt"
	starfmt "github.com/hephbuild/heph/utils/xstarlark/fmt"
	"io"
	"os"
	"path/filepath"
)

type FmtConfig = starfmt.Config

func FmtFix(path string, cfg FmtConfig) error {
	formatted, err := starfmt.Fmt(path, cfg)
	if err != nil {
		if errors.Is(err, starfmt.ErrSkip) {
			return nil
		}

		return err
	}

	f, err := os.CreateTemp(filepath.Dir(path), "")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	defer f.Close()

	_, err = io.WriteString(f, formatted)
	if err != nil {
		return err
	}

	return os.Rename(f.Name(), path)
}

func FmtCheck(path string, cfg FmtConfig) error {
	b, err := os.ReadFile(path)
	if err != nil {
		return err
	}

	actual := string(b)

	expected, err := starfmt.Fmt(path, cfg)
	if err != nil {
		if errors.Is(err, starfmt.ErrSkip) {
			return nil
		}

		return err
	}

	if actual != expected {
		return fmt.Errorf("needs fixing")
	}

	return nil
}

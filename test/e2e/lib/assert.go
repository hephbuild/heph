package lib

import (
	"fmt"
	"os"
	"strings"
)

func FileContent(p string) (string, error) {
	b, err := os.ReadFile(p)
	if err != nil {
		return "", err
	}

	return strings.TrimSpace(string(b)), nil
}

func FileContentEqual(p, expected string) error {
	actual, err := FileContent(p)
	if err != nil {
		return err
	}

	err = AssertEqual(actual, expected)
	if err != nil {
		return fmt.Errorf("%v:\n%v", p, err)
	}

	return nil
}

func AssertEqual(actual, expected string) error {
	if actual != expected {
		return fmt.Errorf("content is not equal, got:\n%v\nexected:\n%v", actual, expected)
	}

	return nil
}

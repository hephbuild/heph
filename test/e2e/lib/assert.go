package lib

import (
	"fmt"
	"strings"
)

func AssertFileContentEqual(p, expected string) error {
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

func AssertEqual[T comparable](actual, expected T) error {
	if actual != expected {
		return fmt.Errorf("> is not equal, got:\n%v\n> expected:\n%v", actual, expected)
	}

	return nil
}

func AssertNotEqual[T comparable](actual, expected T) error {
	if actual == expected {
		return fmt.Errorf("> is equal, got:\n%v", actual)
	}

	return nil
}

func AssertFileContentLinesPrefix(p string, expected []string) error {
	actual, err := FileContent(p)
	if err != nil {
		return err
	}

	err = AssertLinesPrefix(actual, expected)
	if err != nil {
		return fmt.Errorf("%v:\n%v", p, err)
	}

	return nil
}

func AssertLinesPrefix(actual string, expected []string) error {
	lines := strings.Split(actual, "\n")

	if len(lines) != len(expected) {
		return fmt.Errorf("> does not contain same line numbers, got:\n%v\n> expected:\n%v", lines, expected)
	}

	for i := 0; i < len(lines); i++ {
		line := lines[i]
		p := expected[i]

		if !strings.HasPrefix(line, p) {
			return fmt.Errorf("> line %v does not have prefix %v, got:\n%v", i, p, line)

		}
	}

	return nil
}

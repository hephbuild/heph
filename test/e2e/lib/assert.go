package lib

import (
	"fmt"
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

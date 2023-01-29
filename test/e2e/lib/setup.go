package lib

import (
	"fmt"
	"os"
)

func RmHome() error {
	cache, err := HomePath()
	if err != nil {
		return err
	}

	return os.RemoveAll(cache)
}

func PrintConfig() error {
	cmd := command("query", "config")
	cmd.Stderr = os.Stderr
	cmd.Stdout = os.Stdout

	return cmd.Run()
}

func CleanSetup() error {
	d, _ := os.Getwd()
	fmt.Println("Root: ", d)

	err := RmHome()
	if err != nil {
		return err
	}

	err = PrintConfig()
	if err != nil {
		return err
	}

	return nil
}

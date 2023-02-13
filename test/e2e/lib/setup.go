package lib

import (
	"fmt"
	"os"
	"path/filepath"
)

func RmSandbox() error {
	cache, err := HomePath()
	if err != nil {
		return err
	}

	return os.RemoveAll(filepath.Join(cache, "sandbox"))
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

	err := RmCache()
	if err != nil {
		return err
	}

	err = RmSandbox()
	if err != nil {
		return err
	}

	err = PrintConfig()
	if err != nil {
		return err
	}

	return nil
}

package lib

import (
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
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
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, os.Interrupt, syscall.SIGTERM)

	go func() {
		<-sig
		os.Exit(1)
	}()

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

func Fmt() error {
	cmd := command("fmt")
	cmd.Stderr = os.Stderr
	cmd.Stdout = os.Stdout

	return cmd.Run()
}

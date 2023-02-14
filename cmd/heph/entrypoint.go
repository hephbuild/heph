package main

import (
	"context"
	"errors"
	log "heph/hlog"
	"heph/utils"
	"os"
	"os/signal"
	"syscall"
	"time"
)

func execute() error {
	ctx, cancel := context.WithCancel(context.Background())

	sig := make(chan os.Signal, 1)
	signal.Notify(sig, os.Interrupt, syscall.SIGTERM)

	go func() {
		<-sig
		go func() {
			<-time.After(time.Second)
			log.Warnf("Attempting to soft cancel... ctrl+c one more time to force")
		}()
		cancel()

		<-sig
		os.Exit(1)
	}()

	if err := rootCmd.ExecuteContext(ctx); err != nil {
		return err
	}

	return nil
}

func Execute() {
	utils.Seed()

	if err := execute(); err != nil {
		exitCode := 1
		var eerr ErrorWithExitCode
		if errors.As(err, &eerr) {
			exitCode = eerr.ExitCode
			// This is required in case ErrorWithExitCode does not have an Err set, just an ExitCode
			err = eerr.Err
		}
		printHumanError(err)

		os.Exit(exitCode)
	}
}

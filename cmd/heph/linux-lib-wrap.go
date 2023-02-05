package main

import (
	"fmt"
	"github.com/spf13/cobra"
	log "heph/hlog"
	lwl "heph/linux-wrap-lib"
	"path/filepath"
)

var lwpExtra []string

func init() {
	rootCmd.AddCommand(lwpCmd)

	lwpCmd.Flags().StringArrayVar(&lwpExtra, "extra", nil, "extra libs to include")
}

var lwpCmd = &cobra.Command{
	Use:   "lwp",
	Short: "Linux Wrap Pack",
	Args:  cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		binPath := args[0]
		packPath, err := filepath.Abs(filepath.Base(binPath))
		if err != nil {
			return err
		}

		log.Infof("BIN: %v", binPath)

		bin, err := lwl.Pack(binPath, lwpExtra)
		if err != nil {
			return err
		}

		for _, lib := range bin.Libs {
			fmt.Println("LIB:", lib.Path)
		}

		return bin.Pack(packPath)
	},
}

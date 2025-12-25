package cmd

import (
	"fmt"

	"github.com/hephbuild/heph/lib/tref"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func parseTargetRef(s, cwd, root string) (*pluginv1.TargetRef, error) {
	cwp, err := tref.DirToPackage(cwd, root)
	if err != nil {
		return nil, err
	}

	ref, err := tref.ParseInPackage(s, cwp)
	if err != nil {
		return nil, fmt.Errorf("target: %w", err)
	}

	return ref, nil
}

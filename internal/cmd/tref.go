package cmd

import (
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
)

func parseTargetRef(s, cwd, root string) (*pluginv1.TargetRef, error) {
	cwp, err := tref.DirToPackage(cwd, root)
	if err != nil {
		return nil, err
	}

	return tref.ParseInPackage(s, cwp)
}

package bootstrap

import (
	"context"
	"errors"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/hash"
	"github.com/hephbuild/heph/utils/locks"
	"github.com/hephbuild/heph/utils/xfs"
	"os"
	"sort"
	"strings"
)

var toolTemplate = strings.TrimSpace(`
#!/bin/sh
exec heph run TARGET -- "$@" 
`)

func InstallTools(ctx context.Context, root *hroot.State, graph *graph.State, version string) error {
	l := locks.NewFlock("Tools", root.Tmp.Join("tools.lock").Abs())

	err := l.Lock(ctx)
	if err != nil {
		return err
	}

	defer l.Unlock()

	log.Tracef("Installing tools")

	addrs := graph.Tools.Addrs()
	sort.Strings(addrs)

	h := hash.NewHash()
	h.String(version)
	for _, addr := range addrs {
		h.String(addr)
	}

	toolsHash := h.Sum()

	hashPath := root.Tmp.Join("tools_install").Abs()

	b, err := os.ReadFile(hashPath)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}

	if string(b) == toolsHash {
		log.Tracef("tools already up to date")
		return nil
	}

	dir := root.Home.Join("bin")

	err = os.RemoveAll(dir.Abs())
	if err != nil {
		return err
	}

	err = os.MkdirAll(dir.Abs(), os.ModePerm)
	if err != nil {
		return err
	}

	for _, target := range graph.Tools.Slice() {
		log.Tracef("Installing tool %v", target.Addr)

		wrapper := strings.ReplaceAll(toolTemplate, "TARGET", target.Addr)

		tp, err := specs.ParseTargetAddr("", target.Addr)
		if err != nil {
			return err
		}

		err = os.WriteFile(dir.Join(tp.Name).Abs(), []byte(wrapper), os.ModePerm)
		if err != nil {
			return err
		}
	}

	err = xfs.CreateParentDir(hashPath)
	if err != nil {
		return err
	}

	err = os.WriteFile(hashPath, []byte(toolsHash), os.ModePerm)
	if err != nil {
		return err
	}

	log.Infof("Tools installed at %v", dir.Abs())

	return nil
}

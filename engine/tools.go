//go:build ignored

package engine

import (
	"context"
	"errors"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/utils/fs"
	"github.com/hephbuild/heph/utils/hash"
	"os"
	"sort"
	"strings"
)

var toolTemplate = strings.TrimSpace(`
#!/bin/sh
exec heph run TARGET -- "$@" 
`)

func (e *Engine) InstallTools(ctx context.Context) error {
	err := e.toolsLock.Lock(ctx)
	if err != nil {
		return err
	}

	defer e.toolsLock.Unlock()

	log.Tracef("Installing tools")

	fqns := e.tools.FQNs()
	sort.Strings(fqns)

	h := hash.NewHash()
	h.String(e.Config.Version.String)
	for _, fqn := range fqns {
		h.String(fqn)
	}

	toolsHash := h.Sum()

	hashPath := e.HomeDir.Join("tmp", "tools_install").Abs()

	b, err := os.ReadFile(hashPath)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}

	if string(b) == toolsHash {
		log.Tracef("tools already installed")
		return nil
	}

	dir := e.HomeDir.Join("bin")

	err = os.RemoveAll(dir.Abs())
	if err != nil {
		return err
	}

	err = os.MkdirAll(dir.Abs(), os.ModePerm)
	if err != nil {
		return err
	}

	for _, target := range e.tools.Slice() {
		log.Tracef("Installing tool %v", target.FQN)

		wrapper := strings.ReplaceAll(toolTemplate, "TARGET", target.FQN)

		tp, err := targetspec.TargetParse("", target.FQN)
		if err != nil {
			return err
		}

		err = os.WriteFile(dir.Join(tp.Name).Abs(), []byte(wrapper), os.ModePerm)
		if err != nil {
			return err
		}
	}

	err = fs.CreateParentDir(hashPath)
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

package hroot

import (
	"fmt"
	"github.com/hephbuild/heph/log/log"
	fs2 "github.com/hephbuild/heph/utils/fs"
	"os"
)

type State struct {
	Root fs2.Path
	Home fs2.Path
}

func NewState(rootPath string) (*State, error) {
	root := fs2.NewPath(rootPath, "")
	home := root.Join(".heph")

	err := os.MkdirAll(home.Abs(), os.ModePerm)
	if err != nil {
		return nil, fmt.Errorf("create homedir %v: %w", home.Abs(), err)
	}

	log.Tracef("root dir %v", root.Abs())
	log.Tracef("home dir %v", home.Abs())

	return &State{
		Root: root,
		Home: home,
	}, nil
}

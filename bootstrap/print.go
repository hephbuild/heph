package bootstrap

import (
	"fmt"
	"github.com/hephbuild/heph/engine"
	"github.com/hephbuild/heph/utils/fs"
	"io"
	"os"
)

func PrintTargetOutputContent(target *engine.Target, output string) error {
	return targetOutputsFunc(target, output, func(path fs.Path) error {
		f, err := os.Open(path.Abs())
		if err != nil {
			return err
		}
		defer f.Close()

		_, err = io.Copy(os.Stdout, f)
		return err
	})
}

func PrintTargetOutputPaths(target *engine.Target, output string) error {
	return targetOutputsFunc(target, output, func(path fs.Path) error {
		fmt.Println(path.Abs())
		return nil
	})
}

func targetOutputsFunc(target *engine.Target, output string, f func(path fs.Path) error) error {
	paths := target.ActualOutFiles().All()
	if output != "" {
		if !target.ActualOutFiles().HasName(output) {
			return fmt.Errorf("%v: output `%v` does not exist", target.FQN, output)
		}
		paths = target.ActualOutFiles().Name(output)
	} else if len(paths) == 0 {
		return fmt.Errorf("%v: does not output anything", target.FQN)
	}

	for _, path := range paths {
		err := f(path)
		if err != nil {
			return err
		}
	}

	return nil
}

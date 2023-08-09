package bootstrap

import (
	"fmt"
	"github.com/hephbuild/heph/lcache"
	"github.com/hephbuild/heph/utils/xfs"
	"io"
	"os"
)

func PrintTargetOutputContent(target *lcache.Target, output string) error {
	return targetOutputsFunc(target, output, func(path xfs.Path) error {
		f, err := os.Open(path.Abs())
		if err != nil {
			return err
		}
		defer f.Close()

		_, err = io.Copy(os.Stdout, f)
		return err
	})
}

func PrintTargetOutputPaths(target *lcache.Target, output string) error {
	return targetOutputsFunc(target, output, func(path xfs.Path) error {
		fmt.Println(path.Abs())
		return nil
	})
}

func targetOutputsFunc(target *lcache.Target, output string, f func(path xfs.Path) error) error {
	paths := target.ActualOutFiles().All()
	if output != "" {
		if !target.ActualOutFiles().HasName(output) {
			return fmt.Errorf("%v: output `%v` does not exist", target.Addr, output)
		}
		paths = target.ActualOutFiles().Name(output)
	} else if len(paths) == 0 {
		return fmt.Errorf("%v: does not output anything", target.Addr)
	}

	for _, path := range paths.WithRoot(target.OutExpansionRoot().Abs()) {
		err := f(path)
		if err != nil {
			return err
		}
	}

	return nil
}

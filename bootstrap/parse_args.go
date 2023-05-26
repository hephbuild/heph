package bootstrap

import (
	"bufio"
	"github.com/hephbuild/heph/targetspec"
	"os"
	"strings"
)

func BlockReadStdin(args []string) error {
	if HasStdin(args) {
		_, err := parseTargetPathsFromStdin()
		if err != nil {
			return err
		}
	}

	return nil
}

// cache reading stdin
var targetsFromStdin []targetspec.TargetPath

func parseTargetPathsFromStdin() ([]targetspec.TargetPath, error) {
	if targetsFromStdin != nil {
		return targetsFromStdin, nil
	}

	s := bufio.NewScanner(os.Stdin)
	for s.Scan() {
		t := s.Text()
		t = strings.TrimSpace(t)

		if len(t) == 0 {
			continue
		}

		tp, err := targetspec.TargetParse("", t)
		if err != nil {
			return nil, err
		}

		targetsFromStdin = append(targetsFromStdin, tp)
	}

	return targetsFromStdin, nil
}

func HasStdin(args []string) bool {
	return len(args) == 1 && args[0] == "-"
}

func ParseTargetPathsAndArgs(args []string, stdin bool) ([]targetspec.TargetPath, []string, error) {
	var tps []targetspec.TargetPath
	var targs []string
	if stdin && HasStdin(args) {
		// Block and read stdin here to prevent multiple bubbletea running at the same time
		var err error
		tps, err = parseTargetPathsFromStdin()
		if err != nil {
			return nil, nil, err
		}
	} else {
		if len(args) == 0 {
			return nil, nil, nil
		}

		tp, err := targetspec.TargetParse("", args[0])
		if err != nil {
			return nil, nil, err
		}
		tps = []targetspec.TargetPath{tp}

		targs = args[1:]
	}

	return tps, targs, nil
}

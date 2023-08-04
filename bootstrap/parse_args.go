package bootstrap

import (
	"bufio"
	"github.com/hephbuild/heph/specs"
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
var targetsFromStdin []specs.TargetAddr

func parseTargetPathsFromStdin() ([]specs.TargetAddr, error) {
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

		tp, err := specs.ParseTargetAddr("", t)
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

func ParseTargetAddrsAndArgs(args []string, stdin bool) ([]specs.TargetAddr, []string, error) {
	var tps []specs.TargetAddr
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

		tp, err := specs.ParseTargetAddr("", args[0])
		if err != nil {
			return nil, nil, err
		}
		tps = []specs.TargetAddr{tp}

		targs = args[1:]
	}

	return tps, targs, nil
}

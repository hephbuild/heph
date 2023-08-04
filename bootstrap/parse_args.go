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
var targetsFromStdin specs.TargetAddrs

func parseTargetPathsFromStdin() (specs.TargetAddrs, error) {
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

func ParseTargetAddrsAndArgs(args []string, stdin bool) (specs.Matcher, []string, error) {
	var m specs.Matcher
	var targs []string
	if stdin && HasStdin(args) {
		// Block and read stdin here to prevent multiple bubbletea running at the same time
		tps, err := parseTargetPathsFromStdin()
		if err != nil {
			return nil, nil, err
		}

		m = tps
	} else {
		if len(args) == 0 {
			return nil, nil, nil
		}

		var err error
		m, err = specs.ParseMatcher(args[0])
		if err != nil {
			return nil, nil, err
		}

		targs = args[1:]
	}

	return m, targs, nil
}

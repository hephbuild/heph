package pluginexec

import (
	"bytes"
	"context"
	"os/exec"
	"slices"
	"strconv"
	"sync"
	"time"
)

// See https://www.in-ulm.de/~mascheck/various/argmax/

var MAX_ARG_STRLEN = 131072

func removeLongestSrc(env []string) ([]string, bool) {
	maxi := -1
	maxl := -1
	for i, v := range env {
		l := len(v)

		if l > maxl {
			maxi = i
			maxl = l
		}
	}

	if maxi == -1 {
		return env, false
	}

	return slices.Delete(env, maxi, maxi), true
}

func envLength(env []string) int64 {
	l := int64(0)
	for _, v := range env {
		l += int64(len(v))
		l += 2
	}
	l += 2048
	return l
}

func maxArgs() (int64, error) {
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	cmd := exec.CommandContext(ctx, "getconf", "ARG_MAX")
	b, err := cmd.Output()
	if err != nil {
		return 0, err
	}

	maxl, err := strconv.ParseInt(string(bytes.TrimSpace(b)), 10, 64)
	if err != nil {
		return 0, err
	}

	return maxl, nil
}

var maxArgsOnce = sync.OnceValues(maxArgs)

func FilterLongEnv(env []string, args []string) ([]string, error) {
	maxl, err := maxArgsOnce()
	if err != nil {
		return env, err
	}

	if maxl <= 0 {
		return env, nil
	}

	argsl := int64(0)
	for _, arg := range args {
		argsl += int64(len(arg))
	}

	env = slices.DeleteFunc(env, func(v string) bool {
		return len(v) > MAX_ARG_STRLEN
	})

	for envLength(env)+argsl >= maxl {
		var removed bool
		env, removed = removeLongestSrc(env)
		if !removed {
			break
		}
	}

	return env, nil
}

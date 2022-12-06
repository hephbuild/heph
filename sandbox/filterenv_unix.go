package sandbox

import (
	"bytes"
	log "github.com/sirupsen/logrus"
	"heph/utils"
	"os/exec"
	"sort"
	"strconv"
	"strings"
)

// See https://www.in-ulm.de/~mascheck/various/argmax/

var MAX_ARG_STRLEN = 131072

func envSrcKeys(env map[string]string) []string {
	keys := make([]string, 0)
	for k := range env {
		if !strings.HasPrefix(k, "SRC") {
			continue
		}

		keys = append(keys, k)
	}

	sort.SliceStable(keys, func(i, j int) bool {
		return len(keys[i]) < len(keys[j])
	})

	return keys
}

func removeLongestSrc(env map[string]string) bool {
	keys := envSrcKeys(env)

	maxk := ""
	maxl := -1
	for _, k := range keys {
		l := len(k) + len(env[k])

		if l > maxl {
			maxk = k
			maxl = l
		}
	}

	if maxk == "" {
		return false
	}

	log.Debugf("Removing %v from env (> ARG MAX)", maxk)
	delete(env, maxk)
	return true
}

func envLength(env map[string]string) int64 {
	l := int64(0)
	for k, v := range env {
		l += int64(len(k))
		l += int64(len(v))
		l += 4
	}
	l += 2048
	return l
}

func maxArgs() (int64, error) {
	cmd := exec.Command("getconf", "ARG_MAX")
	b, err := cmd.Output()
	if err != nil {
		return 0, err
	}

	max, err := strconv.ParseInt(string(bytes.TrimSpace(b)), 10, 64)
	if err != nil {
		return 0, err
	}

	return max, nil
}

var maxArgsOnce utils.Once[int64]

func FilterLongEnv(env map[string]string, args []string) error {
	max, err := maxArgsOnce.Do(maxArgs)
	if err != nil {
		return err
	}

	if max <= 0 {
		return nil
	}

	argsl := int64(0)
	for _, arg := range args {
		argsl += int64(len(arg))
	}

	for _, k := range envSrcKeys(env) {
		if len(env[k]) > MAX_ARG_STRLEN {
			log.Debugf("Removing %v from env (> MAX_ARG_STRLEN)", k)
			delete(env, k)
		}
	}

	for {
		if envLength(env)+argsl < max {
			break
		}

		if !removeLongestSrc(env) {
			break
		}
	}

	return nil
}

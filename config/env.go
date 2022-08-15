package config

import (
	"os"
	"strings"
)

func ProfilesFromEnv() []string {
	s := os.Getenv("HEPH_PROFILES")
	if s == "" {
		return nil
	}

	return strings.Split(s, ",")
}

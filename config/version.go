package config

import (
	"github.com/coreos/go-semver/semver"
	"strings"
)

type Version struct {
	String string
	Semver *semver.Version
	GTE    bool
}

func (e Version) MarshalYAML() (interface{}, error) {
	return e.String, nil
}

func (e *Version) UnmarshalYAML(unmarshal func(interface{}) error) error {
	err := unmarshal(&e.String)
	if err != nil {
		return err
	}

	e.String = strings.TrimSpace(e.String)

	version := e.String
	if strings.HasPrefix(version, ">=") {
		e.GTE = true
		version = strings.TrimPrefix(version, ">=")
		version = strings.TrimSpace(version)
	}

	e.Semver, err = semver.NewVersion(version)
	if err != nil {
		if e.GTE {
			// If its gte, it has to be a semver, report errors if not
			return err
		}
	}

	return nil
}

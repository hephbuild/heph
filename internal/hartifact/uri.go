package hartifact

import (
	"fmt"
	"strings"
)

func ParseURI(uri string) (string, string, error) {
	scheme, rest, ok := strings.Cut(uri, "://")
	if !ok {
		return "", "", fmt.Errorf("invalid URI: %s", uri)
	}

	return scheme, rest, nil
}

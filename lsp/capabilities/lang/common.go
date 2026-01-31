package lang

import "strings"

func addProtocol(uri string) string {
	if !strings.HasPrefix(uri, "file://") {
		uri = "file://" + uri
	}
	return uri
}

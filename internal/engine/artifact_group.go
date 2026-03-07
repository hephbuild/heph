package engine

import "github.com/hephbuild/heph/lib/pluginsdk"

type artifactGroupMap struct {
	pluginsdk.Artifact
	group string
}

func (a artifactGroupMap) GetGroup() string {
	return a.group
}

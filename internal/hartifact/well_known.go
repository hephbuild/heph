package hartifact

import (
	"github.com/hephbuild/heph/lib/pluginsdk"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func FindOutputs(artifacts []pluginsdk.Artifact, group string) []pluginsdk.Artifact {
	out := make([]pluginsdk.Artifact, 0, 1)
	for _, artifact := range artifacts {
		if artifact.GetType() != pluginv1.Artifact_TYPE_OUTPUT {
			continue
		}

		if artifact.GetGroup() != group {
			continue
		}

		out = append(out, artifact)
	}
	return out
}

package hartifact

import (
	"iter"
	"slices"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func FindOutputs(artifacts []*pluginv1.Artifact, group string) []*pluginv1.Artifact {
	return findOutputs(slices.All(artifacts), group)
}

func findOutputs(artifacts iter.Seq2[int, *pluginv1.Artifact], group string) []*pluginv1.Artifact {
	out := make([]*pluginv1.Artifact, 0, 1)
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

package lcache

import (
	"context"
	"github.com/hephbuild/heph/artifacts"
)

type ArtifactProducer interface {
	Gen(ctx context.Context, gctx *ArtifactGenContext) error
}

type ArtifactWithProducer interface {
	artifacts.Artifact
	ArtifactProducer
}

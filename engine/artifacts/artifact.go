package artifacts

import (
	"context"
	"errors"
	"fmt"
	"heph/utils/fs"
	"os"
	"path/filepath"
)

var Skip = errors.New("skip artifact")

type container struct {
	Producer
	name        string
	displayName string
	genRequired bool
}

func (a container) Name() string {
	return a.name
}

func (a container) DisplayName() string {
	return a.displayName
}

func (a container) GenRequired() bool {
	return a.genRequired
}

func New(name, displayName string, genRequired bool, artifact Producer) Artifact {
	return container{
		Producer:    artifact,
		name:        name,
		displayName: displayName,
		genRequired: genRequired,
	}
}

type GenContext struct {
	ArtifactPath string

	OutRoot     string
	LogFilePath string
}

type Producer interface {
	Gen(ctx context.Context, gctx GenContext) error
}

type Artifact interface {
	Producer
	Name() string
	DisplayName() string
	GenRequired() bool
}

type Func struct {
	Func func(ctx context.Context, gctx GenContext) error
}

func (a Func) Gen(ctx context.Context, gctx GenContext) error {
	return a.Func(ctx, gctx)
}

func GenArtifact(ctx context.Context, dir string, a Artifact, gctx GenContext) (string, error) {
	p := filepath.Join(dir, a.Name())
	tmpp := fs.ProcessUniquePath(p)
	defer os.Remove(tmpp)

	gctx.ArtifactPath = tmpp

	err := a.Gen(ctx, gctx)
	if err != nil {
		if errors.Is(err, Skip) {
			if a.GenRequired() {
				return "", fmt.Errorf("%v is required, but returned: %w", a.Name(), err)
			}
			return "", nil
		}
		return "", fmt.Errorf("%v: %w", a.Name(), err)
	}

	if !fs.PathExists(tmpp) {
		return "", fmt.Errorf("%v did not produce output", a.Name())
	}

	err = os.Rename(tmpp, p)
	if err != nil {
		return "", err
	}

	return p, nil
}

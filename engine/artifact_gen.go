package engine

import (
	"compress/gzip"
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/engine/artifacts"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/utils/fs"
	"github.com/hephbuild/heph/utils/mds"
	"io"
	"os"
	"path/filepath"
)

var ArtifactSkip = errors.New("skip artifact")

type ArtifactGenContext struct {
	w              io.WriteCloser
	accessedWriter bool
}

func (g *ArtifactGenContext) Writer() io.Writer {
	g.accessedWriter = true
	return g.w
}

type ArtifactProducer interface {
	Gen(ctx context.Context, gctx *ArtifactGenContext) error
}

type ArtifactWithProducer interface {
	artifacts.Artifact
	ArtifactProducer
}

type artifactProducer struct {
	artifacts.Artifact
	ArtifactProducer
}

func GenArtifact(ctx context.Context, dir string, a ArtifactWithProducer, compress bool) (string, error) {
	shouldCompress := a.Compressible() && compress

	p := filepath.Join(dir, a.FileName())
	if shouldCompress {
		p = filepath.Join(dir, a.GzFileName())
	}

	tmpp := fs.ProcessUniquePath(p)
	defer os.Remove(tmpp)

	f, err := os.Create(tmpp)
	if err != nil {
		return "", err
	}
	defer f.Close()

	go func() {
		<-ctx.Done()
		_ = f.Close()
	}()

	gctx := ArtifactGenContext{}
	gctx.w = f

	if shouldCompress {
		gw := gzip.NewWriter(f)
		defer gw.Close()

		gctx.w = gw
	}

	err = a.Gen(ctx, &gctx)
	if err != nil {
		if errors.Is(err, ArtifactSkip) {
			if a.GenRequired() {
				return "", fmt.Errorf("%v is required, but returned: %w", a.Name(), err)
			}
			return "", nil
		}
		return "", fmt.Errorf("%v: %w", a.Name(), err)
	}

	if !gctx.accessedWriter {
		return "", fmt.Errorf("%v did not produce output", a.Name())
	}

	_ = gctx.w.Close()
	_ = f.Close()

	err = os.Rename(tmpp, p)
	if err != nil {
		return "", err
	}

	return p, nil
}

// orderedArtifactProducers returns artifact in hashing order
// For hashing to work properly, support_files tar must go first, then support_files hash
// then the other artifacts
func (e *LocalCacheState) orderedArtifactProducers(t *Target, outRoot, logFilePath string) []ArtifactWithProducer {
	arts := t.Artifacts

	all := make([]ArtifactWithProducer, 0, len(arts.Out)+2)
	all = append(all, artifactProducer{arts.Log, logArtifact{
		LogFilePath: logFilePath,
	}})
	names := mds.Keys(arts.Out)
	names = targetspec.SortOutputsForHashing(names)
	for _, name := range names {
		a := arts.Out[name]
		all = append(all, artifactProducer{a.Tar(), outTarArtifact{
			Target:  t,
			Output:  name,
			OutRoot: outRoot,
		}})
		all = append(all, artifactProducer{a.Hash(), hashOutputArtifact{
			LocalState: e,
			Target:     t,
			Output:     name,
		}})
	}
	all = append(all, artifactProducer{arts.Manifest, manifestArtifact{
		LocalState: e,
		Target:     t,
	}})
	all = append(all, artifactProducer{arts.InputHash, hashInputArtifact{
		LocalState: e,
		Target:     t,
	}})
	return all
}

func (e *LocalCacheState) GenArtifacts(ctx context.Context, dir string, target *Target, allArtifacts []ArtifactWithProducer, compress bool) error {
	for _, artifact := range allArtifacts {
		err := target.cacheLocks[artifact.Name()].Lock(ctx)
		if err != nil {
			return fmt.Errorf("lock %v %v: %w", target.FQN, artifact.Name(), err)
		}
	}
	defer func() {
		for _, artifact := range allArtifacts {
			err := target.cacheLocks[artifact.Name()].Unlock()
			if err != nil {
				log.Errorf("unlock %v %v: %v", target.FQN, artifact.Name(), err)
			}
		}
	}()

	for _, artifact := range allArtifacts {
		_, err := GenArtifact(ctx, dir, artifact, compress)
		if err != nil {
			return fmt.Errorf("genartifact %v: %w", artifact.Name(), err)
		}
	}

	return nil
}

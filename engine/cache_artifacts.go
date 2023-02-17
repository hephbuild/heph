package engine

import (
	"context"
	"heph/engine/artifacts"
	log "heph/hlog"
	"heph/targetspec"
	"heph/utils/fs"
	"heph/utils/tar"
	"os"
)

type outTarArtifact struct {
	Target *Target
	Output string
}

func (a outTarArtifact) Gen(ctx context.Context, gctx artifacts.GenContext) error {
	target := a.Target

	var paths fs.Paths
	if a.Output == targetspec.SupportFilesOutput {
		paths = target.ActualSupportFiles()
	} else {
		paths = target.ActualOutFiles().Name(a.Output)
	}
	log.Tracef("Creating archive %v %v", target.FQN, a.Output)

	files := make([]tar.TarFile, 0)
	for _, file := range paths {
		if err := ctx.Err(); err != nil {
			return err
		}

		file := file.WithRoot(gctx.OutRoot)

		files = append(files, tar.TarFile{
			From: file.Abs(),
			To:   file.RelRoot(),
		})
	}

	err := tar.Tar(ctx, files, gctx.ArtifactPath)
	if err != nil {
		return err
	}

	return nil
}

type hashOutputArtifact struct {
	Engine *Engine
	Target *Target
	Output string
}

func (a hashOutputArtifact) Gen(ctx context.Context, gctx artifacts.GenContext) error {
	outputHash := a.Engine.hashOutput(a.Target, a.Output)

	return fs.WriteFileSync(gctx.ArtifactPath, []byte(outputHash), os.ModePerm)
}

type hashInputArtifact struct {
	Engine *Engine
	Target *Target
}

func (a hashInputArtifact) Gen(ctx context.Context, gctx artifacts.GenContext) error {
	inputHash := a.Engine.hashInput(a.Target)

	return fs.WriteFileSync(gctx.ArtifactPath, []byte(inputHash), os.ModePerm)
}

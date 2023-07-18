package engine

import (
	"context"
	"encoding/json"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/lcache"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/utils/tar"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/utils/xsync"
	"io"
	"os"
	"os/exec"
	"strings"
	"time"
)

type outTarArtifact struct {
	Target  *Target
	Output  string
	OutRoot string
}

func (a outTarArtifact) Gen(ctx context.Context, gctx *lcache.ArtifactGenContext) error {
	target := a.Target

	var paths xfs.Paths
	if a.Output == targetspec.SupportFilesOutput {
		paths = target.ActualSupportFiles()
	} else {
		paths = target.ActualOutFiles().Name(a.Output)
	}
	log.Tracef("Creating archive %v %v", target.FQN, a.Output)

	files := make([]tar.File, 0)
	for _, file := range paths {
		if err := ctx.Err(); err != nil {
			return err
		}

		file := file.WithRoot(a.OutRoot)

		files = append(files, tar.File{
			From: file.Abs(),
			To:   file.RelRoot(),
		})
	}

	err := tar.Tar(gctx.Writer(), files)
	if err != nil {
		return err
	}

	return nil
}

type hashOutputArtifact struct {
	LocalState *lcache.LocalCacheState
	Target     graph.Targeter
	Output     string
}

func (a hashOutputArtifact) Gen(ctx context.Context, gctx *lcache.ArtifactGenContext) error {
	outputHash, err := a.LocalState.HashOutput(a.Target, a.Output)
	if err != nil {
		return err
	}

	_, err = io.WriteString(gctx.Writer(), outputHash)
	return err
}

type hashInputArtifact struct {
	LocalState *lcache.LocalCacheState
	Target     graph.Targeter
}

func (a hashInputArtifact) Gen(ctx context.Context, gctx *lcache.ArtifactGenContext) error {
	inputHash, err := a.LocalState.HashInput(a.Target)
	if err != nil {
		return err
	}

	_, err = io.WriteString(gctx.Writer(), inputHash)
	return err
}

type logArtifact struct {
	LogFilePath string
}

func (a logArtifact) Gen(ctx context.Context, gctx *lcache.ArtifactGenContext) error {
	if a.LogFilePath == "" {
		return lcache.ArtifactSkip
	}

	f, err := os.Open(a.LogFilePath)
	if err != nil {
		return err
	}
	defer f.Close()

	_, err = io.Copy(gctx.Writer(), f)
	return err
}

type manifestArtifact struct {
	LocalState *lcache.LocalCacheState
	Target     graph.Targeter
}

type ManifestData struct {
	GitCommit  string                       `json:"git_commit,omitempty"`
	GitRef     string                       `json:"git_ref,omitempty"`
	InputHash  string                       `json:"input_hash,omitempty"`
	DepsHashes map[string]map[string]string `json:"deps_hashes,omitempty"`
	OutHashes  map[string]string            `json:"out_hashes,omitempty"`
	Timestamp  time.Time                    `json:"timestamp"`
}

func (a manifestArtifact) git(args ...string) string {
	cmd := exec.Command("git", args...)
	b, _ := cmd.Output()

	return strings.TrimSpace(string(b))
}

var gitCommitOnce xsync.Once[string]
var gitRefOnce xsync.Once[string]

func (a manifestArtifact) Gen(ctx context.Context, gctx *lcache.ArtifactGenContext) error {
	inputHash, err := a.LocalState.HashInput(a.Target)
	if err != nil {
		return err
	}

	d := ManifestData{
		GitCommit: gitCommitOnce.MustDo(func() (string, error) {
			return a.git("rev-parse", "HEAD"), nil
		}),
		GitRef: gitRefOnce.MustDo(func() (string, error) {
			return a.git("rev-parse", "--abbrev-ref", "HEAD"), nil
		}),
		InputHash:  inputHash,
		DepsHashes: map[string]map[string]string{},
		OutHashes:  map[string]string{},
		Timestamp:  time.Now(),
	}

	e := a.LocalState

	target := a.Target.GraphTarget()

	allDeps := target.Deps.All().Merge(target.HashDeps)
	for _, dep := range allDeps.Targets {
		if !dep.Target.Out.HasName(dep.Output) {
			continue
		}
		if d.DepsHashes[dep.Target.FQN] == nil {
			d.DepsHashes[dep.Target.FQN] = map[string]string{}
		}
		var err error
		d.DepsHashes[dep.Target.FQN][dep.Output], err = e.HashOutput(e.Targets.Find(dep.Target.FQN), dep.Output)
		if err != nil {
			return err
		}
	}

	for _, name := range target.OutWithSupport.Names() {
		var err error
		d.OutHashes[name], err = e.HashOutput(a.Target, name)
		if err != nil {
			return err
		}
	}

	enc := json.NewEncoder(gctx.Writer())
	return enc.Encode(d)
}

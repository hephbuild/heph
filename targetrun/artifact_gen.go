package targetrun

import (
	"context"
	"encoding/json"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/lcache"
	"github.com/hephbuild/heph/log/log"
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
	Target    *lcache.Target
	Name      string
	Paths     xfs.RelPaths
	OutRoot   string
	SkipEmpty bool
	OnStatErr func(err error) (bool, error)
}

func (a outTarArtifact) Gen(ctx context.Context, gctx *lcache.ArtifactGenContext) error {
	target := a.Target

	log.Tracef("Creating archive %v %v", target.Addr, a.Name)

	paths := a.Paths.WithRoot(a.OutRoot)

	files := make([]tar.File, 0, len(paths))
	var size int64
	for _, file := range paths {
		if err := ctx.Err(); err != nil {
			return err
		}

		info, err := os.Lstat(file.Abs())
		if err != nil {
			return err
		}

		if info != nil {
			size += info.Size()
			size += tar.HeaderOverhead
		}

		files = append(files, tar.File{
			From: file.Abs(),
			To:   file.RelRoot(),
		})
	}

	if a.SkipEmpty && len(files) == 0 {
		return lcache.ArtifactSkip
	}

	gctx.EstimatedWriteSize(size)

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

	info, _ := f.Stat()
	if info != nil {
		gctx.EstimatedWriteSize(info.Size())
	}

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

func (a manifestArtifact) git(ctx context.Context, args ...string) string {
	cmd := exec.CommandContext(ctx, "git", args...)
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
			return a.git(ctx, "rev-parse", "HEAD"), nil
		}),
		GitRef: gitRefOnce.MustDo(func() (string, error) {
			return a.git(ctx, "rev-parse", "--abbrev-ref", "HEAD"), nil
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
		if d.DepsHashes[dep.Target.Addr] == nil {
			d.DepsHashes[dep.Target.Addr] = map[string]string{}
		}
		var err error
		d.DepsHashes[dep.Target.Addr][dep.Output], err = e.HashOutput(e.Targets.FindT(dep.Target), dep.Output)
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

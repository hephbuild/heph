package engine

import (
	"archive/tar"
	"bytes"
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	"hash"
	"io"
	"os"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hproto/hashpb"

	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/htar"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hfs"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/zeebo/xxh3"
)

func (e *Engine) hashout(ctx context.Context, ref *pluginv1.TargetRef, artifact *pluginv1.Artifact) (string, error) {
	var h interface {
		hash.Hash
		io.StringWriter
	}
	if enableHashDebug() {
		h = newHashWithDebug(xxh3.New(), strings.TrimPrefix(tref.Format(ref), "//")+"_hashout_"+artifact.GetName(), "")
	} else {
		h = xxh3.New()
	}

	_, _ = h.WriteString(artifact.GetGroup())
	_, _ = h.WriteString(artifact.GetName())
	_, _ = h.WriteString(strconv.Itoa(int(artifact.GetType())))

	for file, err := range hartifact.FilesReader(ctx, artifact) {
		if err != nil {
			return "", err
		}

		_, _ = h.WriteString(file.Path)

		_, err = io.Copy(h, file)
		if err != nil {
			return "", err
		}

		_ = file.Close()
	}

	return hex.EncodeToString(h.Sum(nil)), nil
}

func (e *Engine) targetDirName(ref *pluginv1.TargetRef) string {
	if len(ref.GetArgs()) == 0 {
		return "__" + ref.GetName()
	}

	h := xxh3.New()
	hashpb.Hash(h, ref, tref.OmitHashPb)

	return "__" + ref.GetName() + "_" + hex.EncodeToString(h.Sum(nil))
}

func (e *Engine) CacheLocally(
	ctx context.Context,
	def *LightLinkedTarget,
	hashin string,
	sandboxArtifacts []ExecuteResultArtifact,
) ([]ExecuteResultArtifact, *hartifact.Manifest, error) {
	step, ctx := hstep.New(ctx, "Caching...")
	defer step.Done()

	cachedir := hfs.At(e.Cache, def.GetRef().GetPackage(), e.targetDirName(def.GetRef()), hashin)

	cacheArtifacts := make([]ExecuteResultArtifact, 0, len(sandboxArtifacts))

	for _, artifact := range sandboxArtifacts {
		if artifact.GetType() == pluginv1.Artifact_TYPE_MANIFEST_V1 {
			continue
		}

		if artifact.HasFile() {
			content := artifact.GetFile()

			artifact.SetName(artifact.GetName() + ".tar")
			tarf, err := hfs.Create(cachedir, artifact.GetName())
			if err != nil {
				return nil, nil, err
			}
			defer tarf.Close()
			p := htar.NewPacker(tarf)

			sourcefs := hfs.NewOS(content.GetSourcePath())

			f, err := hfs.Open(sourcefs, "")
			if err != nil {
				return nil, nil, err
			}
			defer f.Close()

			err = p.WriteFile(f, content.GetOutPath())
			if err != nil {
				return nil, nil, err
			}

			_ = f.Close()
			_ = tarf.Close()

			artifact.SetTarPath(tarf.Name())
		}
		if artifact.HasRaw() {
			content := artifact.GetRaw()

			artifact.SetName(artifact.GetName() + ".tar")
			tarf, err := hfs.Create(cachedir, artifact.GetName())
			if err != nil {
				return nil, nil, err
			}
			defer tarf.Close()
			p := htar.NewPacker(tarf)

			mode := int64(os.ModePerm)
			if content.GetX() {
				mode |= 0111 // executable
			}

			err = p.Write(bytes.NewReader(content.GetData()), &tar.Header{
				Typeflag: tar.TypeReg,
				Name:     content.GetPath(),
				Size:     int64(len(content.GetData())),
				Mode:     mode,
			})
			if err != nil {
				return nil, nil, err
			}

			_ = tarf.Close()

			artifact.SetTarPath(tarf.Name())
		}

		fromPath, err := hartifact.Path(artifact.Artifact)
		if err != nil {
			return nil, nil, err
		}

		if fromPath == "" {
			return nil, nil, fmt.Errorf("artifact %s has no path", artifact.GetName())
		}

		var prefix string
		switch artifact.GetType() {
		case pluginv1.Artifact_TYPE_OUTPUT:
			prefix = "out_"
		case pluginv1.Artifact_TYPE_LOG:
			prefix = "log_"
		case pluginv1.Artifact_TYPE_OUTPUT_LIST_V1, pluginv1.Artifact_TYPE_MANIFEST_V1, pluginv1.Artifact_TYPE_UNSPECIFIED:
			fallthrough
		default:
			return nil, nil, fmt.Errorf("invalid artifact type: %s", artifact.GetType())
		}

		artifact.SetName(prefix + artifact.GetName())
		fromfs := hfs.NewOS(fromPath)
		tofs := hfs.At(cachedir, artifact.GetName())

		if false && strings.HasPrefix(fromfs.Path(), e.Home.Path()) {
			// TODO: there was a bug here where when a target was running in tree, it would move things out from tree
			err = hfs.Move(fromfs, tofs)
			if err != nil {
				return nil, nil, fmt.Errorf("move: %w", err)
			}
		} else {
			err = hfs.Copy(fromfs, tofs)
			if err != nil {
				return nil, nil, fmt.Errorf("copy: %w", err)
			}
		}

		cachedArtifact, err := hartifact.Relocated(artifact.Artifact, tofs.Path())
		if err != nil {
			return nil, nil, fmt.Errorf("relocated: %w", err)
		}

		cacheArtifacts = append(cacheArtifacts, ExecuteResultArtifact{
			Hashout:  artifact.Hashout,
			Artifact: cachedArtifact,
		})
	}

	m := hartifact.Manifest{
		Version:   "v1",
		Target:    tref.Format(def.GetRef()),
		CreatedAt: time.Now(),
		Hashin:    hashin,
	}
	for _, artifact := range cacheArtifacts {
		martifact, err := hartifact.ProtoArtifactToManifest(artifact.Hashout, artifact.Artifact)
		if err != nil {
			return nil, nil, err
		}

		m.Artifacts = append(m.Artifacts, martifact)
	}

	manifestArtifact, err := hartifact.WriteManifest(cachedir, m)
	if err != nil {
		return nil, nil, err
	}

	cacheArtifacts = append(cacheArtifacts, ExecuteResultArtifact{
		Artifact: manifestArtifact,
	})

	return cacheArtifacts, &m, nil
}

func (e *Engine) ClearCacheLocally(
	ctx context.Context,
	ref *pluginv1.TargetRef,
	hashin string,
) error {
	step, ctx := hstep.New(ctx, "Clearing...")
	defer step.Done()

	cachedir := hfs.At(e.Cache, ref.GetPackage(), e.targetDirName(ref), hashin)

	return cachedir.RemoveAll("")
}

func keyRefOutputs(ref *pluginv1.TargetRef, outputs []string) string {
	switch len(outputs) {
	case 0:
		outputs = nil
	case 1:
		// noop
	default:
		outputs = slices.Clone(outputs)
		slices.Sort(outputs)
	}

	return refKey(ref) + fmt.Sprintf("%#v", outputs)
}

func (e *Engine) ResultFromLocalCache(ctx context.Context, def *LightLinkedTarget, outputs []string, hashin string) (*ExecuteResult, bool, error) {
	ctx, span := tracer.Start(ctx, "ResultFromLocalCache")
	defer span.End()

	res, ok, err := e.resultFromLocalCacheInner(ctx, def, outputs, hashin)
	if err != nil {
		// if the file doesnt exist, thats not an error, just means the cache doesnt exist locally
		if errors.Is(err, hfs.ErrNotExist) {
			return nil, false, nil
		}

		return nil, false, err
	}

	return res, ok, nil
}

func (e *Engine) resultFromLocalCacheInner(ctx context.Context, def *LightLinkedTarget, outputs []string, hashin string) (*ExecuteResult, bool, error) {
	dirfs := hfs.At(e.Cache, def.GetRef().GetPackage(), e.targetDirName(def.GetRef()), hashin)

	manifest, manifestArtifact, err := hartifact.ManifestFromFS(dirfs)
	if err != nil {
		return nil, false, fmt.Errorf("ManifestFromFS: %w", err)
	}

	artifacts := make([]hartifact.ManifestArtifact, 0, len(outputs))
	for _, output := range outputs {
		outputArtifacts := manifest.GetArtifacts(output)

		artifacts = append(artifacts, outputArtifacts...)
	}

	execArtifacts := make([]ExecuteResultArtifact, 0, len(artifacts))
	for _, artifact := range artifacts {
		if !hfs.Exists(dirfs, artifact.Name) {
			return nil, false, nil
		}

		partifact, err := hartifact.ManifestArtifactToProto(artifact, dirfs.Path(artifact.Name))
		if err != nil {
			return nil, false, fmt.Errorf("ManifestArtifactToProto: %w", err)
		}

		execArtifacts = append(execArtifacts, ExecuteResultArtifact{
			Hashout:  artifact.Hashout,
			Artifact: partifact,
		})
	}

	execArtifacts = append(execArtifacts, ExecuteResultArtifact{
		Artifact: manifestArtifact,
	})

	return ExecuteResult{
		Def:       def,
		Hashin:    manifest.Hashin,
		Artifacts: execArtifacts,
	}.Sorted(), true, nil
}

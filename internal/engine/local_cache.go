package engine

import (
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"slices"
	"time"

	"github.com/hephbuild/heph/plugin/tref"
	"google.golang.org/protobuf/proto"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hlocks"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/zeebo/xxh3"
)

func (e *Engine) hashout(ctx context.Context, artifact *pluginv1.Artifact) (string, error) {
	h := xxh3.New()
	writeProto := func(v proto.Message) error {
		return stableProtoHashEncode(h, v, nil)
	}

	err := writeProto(artifact)
	if err != nil {
		return "", err
	}

	r, err := hartifact.Reader(ctx, artifact)
	if err != nil {
		return "", err
	}
	defer r.Close()

	_, err = io.Copy(h, r)
	if err != nil {
		return "", err
	}

	return hex.EncodeToString(h.Sum(nil)), nil
}

func (e *Engine) targetDirName(ref *pluginv1.TargetRef) string {
	if len(ref.GetArgs()) == 0 {
		return "__" + ref.GetName()
	}

	h := xxh3.New()
	ref.HashPB(h, nil)

	return "__" + ref.GetName() + "_" + hex.EncodeToString(h.Sum(nil))
}

func (e *Engine) CacheLocally(ctx context.Context, def *LightLinkedTarget, hashin string, sandboxArtifacts []ExecuteResultArtifact) ([]ExecuteResultArtifact, error) {
	// TODO: locks

	cachedir := hfs.At(e.Cache, def.GetRef().GetPackage(), e.targetDirName(def.GetRef()), hashin)

	cacheArtifacts := make([]ExecuteResultArtifact, 0, len(sandboxArtifacts))

	for _, artifact := range sandboxArtifacts {
		scheme, rest, err := hartifact.ParseURI(artifact.Uri)
		if err != nil {
			return nil, err
		}

		var prefix string
		switch artifact.Type {
		case pluginv1.Artifact_TYPE_OUTPUT:
			prefix = "out_"
		case pluginv1.Artifact_TYPE_LOG:
			prefix = "log_"
		case pluginv1.Artifact_TYPE_OUTPUT_LIST_V1, pluginv1.Artifact_TYPE_MANIFEST_V1, pluginv1.Artifact_TYPE_UNSPECIFIED:
			fallthrough
		default:
			return nil, fmt.Errorf("invalid artifact type: %s", artifact.Type)
		}

		var cachedArtifact *pluginv1.Artifact

		switch scheme {
		case "file":
			name := prefix + artifact.Name
			fromfs := hfs.NewOS(rest)
			tofs := hfs.At(cachedir, name)

			err = hfs.Move(fromfs, tofs)
			if err != nil {
				return nil, err
			}

			cachedArtifact = &pluginv1.Artifact{
				Group:    artifact.Group,
				Name:     name,
				Type:     artifact.Type,
				Encoding: artifact.Encoding,
				Uri:      "file://" + tofs.Path(),
			}
		default:
			return nil, fmt.Errorf("unsupprted scheme: %s", scheme)
		}

		hashout := artifact.Hashout
		if hashout == "" && artifact.Type == pluginv1.Artifact_TYPE_OUTPUT {
			hashout, err = e.hashout(ctx, cachedArtifact)
			if err != nil {
				return nil, err
			}
		}

		cacheArtifacts = append(cacheArtifacts, ExecuteResultArtifact{
			Hashout:  hashout,
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
		m.Artifacts = append(m.Artifacts, hartifact.ManifestArtifact{
			Hashout:  artifact.Hashout,
			Group:    artifact.Group,
			Name:     artifact.Name,
			Type:     artifact.Type,
			Encoding: artifact.Encoding,
		})
	}

	manifestArtifact, err := hartifact.NewManifestArtifact(cachedir, m)
	if err != nil {
		return nil, err
	}

	cacheArtifacts = append(cacheArtifacts, ExecuteResultArtifact{
		Artifact: manifestArtifact,
	})

	return cacheArtifacts, nil
}

func keyRefOutputs(ref *pluginv1.TargetRef, outputs []string) string {
	if len(outputs) == 0 {
		outputs = nil
	} else {
		outputs = slices.Clone(outputs)
		slices.Sort(outputs)
	}

	return tref.Format(ref) + fmt.Sprintf("%#v", outputs)
}

func (e *Engine) ResultFromLocalCache(ctx context.Context, def *LightLinkedTarget, outputs []string, hashin string) (*ExecuteResult, bool, error) {
	ctx, span := tracer.Start(ctx, "ResultFromLocalCache")
	defer span.End()

	multi := hlocks.NewMulti()

	res, ok, err := e.resultFromLocalCacheInner(ctx, def, outputs, hashin, multi)
	if err != nil {
		if err := multi.UnlockAll(); err != nil {
			hlog.From(ctx).Error(fmt.Sprintf("failed to unlock: %v", err))
		}

		// if the file doesnt exist, thats not an error, just means the cache doesnt exist locally
		if errors.Is(err, hfs.ErrNotExist) {
			return nil, false, nil
		}

		return nil, false, err
	}

	return res, ok, nil
}

func (e *Engine) resultFromLocalCacheInner(ctx context.Context, def *LightLinkedTarget, outputs []string, hashin string, locks *hlocks.Multi) (*ExecuteResult, bool, error) {
	dirfs := hfs.At(e.Cache, def.GetRef().GetPackage(), e.targetDirName(def.GetRef()), hashin)

	{
		l := hlocks.NewFlock2(dirfs, "", hartifact.ManifestName, false)
		err := l.RLock(ctx)
		if err != nil {
			return nil, false, fmt.Errorf("flock manifest: %w", err)
		}
		locks.Add(l.RUnlock)
	}

	manifest, err := hartifact.ManifestFromFS(dirfs)
	if err != nil {
		return nil, false, fmt.Errorf("ManifestFromFS: %w", err)
	}

	var artifacts []hartifact.ManifestArtifact
	for _, output := range outputs {
		outputArtifacts := manifest.GetArtifacts(output)

		artifacts = append(artifacts, outputArtifacts...)
	}

	for _, artifact := range artifacts {
		l := hlocks.NewFlock2(dirfs, "", artifact.Name, false)
		err := l.RLock(ctx)
		if err != nil {
			return nil, false, fmt.Errorf("flock artifact: %w", err)
		}
		locks.Add(l.RUnlock)
	}

	execArtifacts := make([]ExecuteResultArtifact, 0, len(artifacts))
	for _, artifact := range artifacts {
		execArtifacts = append(execArtifacts, ExecuteResultArtifact{
			Hashout: artifact.Hashout,
			Artifact: &pluginv1.Artifact{
				Group:    artifact.Group,
				Name:     artifact.Name,
				Type:     artifact.Type,
				Encoding: artifact.Encoding,
				Uri:      "file://" + dirfs.Path(artifact.Name),
			},
		})
	}

	manifestArtifact, err := hartifact.NewManifestArtifact(dirfs, manifest)
	if err != nil {
		return nil, false, fmt.Errorf("NewManifestArtifact: %w", err)
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

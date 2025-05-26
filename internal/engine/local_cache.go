package engine

import (
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"slices"
	"strings"
	"time"

	"github.com/hephbuild/heph/plugin/tref"
	"google.golang.org/protobuf/proto"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hfs"
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

func (e *Engine) CacheLocally(ctx context.Context, def *LightLinkedTarget, hashin string, sandboxArtifacts []ExecuteResultArtifact) ([]ExecuteResultArtifact, *hartifact.Manifest, error) {
	cachedir := hfs.At(e.Cache, def.GetRef().GetPackage(), e.targetDirName(def.GetRef()), hashin)

	cacheArtifacts := make([]ExecuteResultArtifact, 0, len(sandboxArtifacts))

	for _, artifact := range sandboxArtifacts {
		var prefix string
		switch artifact.Type {
		case pluginv1.Artifact_TYPE_OUTPUT:
			prefix = "out_"
		case pluginv1.Artifact_TYPE_LOG:
			prefix = "log_"
		case pluginv1.Artifact_TYPE_OUTPUT_LIST_V1, pluginv1.Artifact_TYPE_MANIFEST_V1, pluginv1.Artifact_TYPE_UNSPECIFIED:
			fallthrough
		default:
			return nil, nil, fmt.Errorf("invalid artifact type: %s", artifact.Type)
		}

		name := prefix + artifact.Name
		fromPath, err := hartifact.Path(artifact.Artifact)
		if err != nil {
			return nil, nil, err
		}

		tofs := hfs.At(cachedir, name)

		if fromPath == "" {
			r, err := hartifact.FileReader(ctx, artifact.Artifact)
			if err != nil {
				return nil, nil, err
			}
			defer r.Close()

			f, err := hfs.Create(tofs, "")
			if err != nil {
				return nil, nil, err
			}
			defer f.Close()

			_, err = io.Copy(f, r)
			if err != nil {
				return nil, nil, err
			}
			_ = r.Close()
			_ = f.Close()
		} else {
			fromfs := hfs.NewOS(fromPath)

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
		}

		cachedArtifact, err := hartifact.Relocated(artifact.Artifact, tofs.Path())
		if err != nil {
			return nil, nil, fmt.Errorf("relocated: %w", err)
		}

		hashout := artifact.Hashout
		if hashout == "" && artifact.Type == pluginv1.Artifact_TYPE_OUTPUT {
			var err error
			hashout, err = e.hashout(ctx, cachedArtifact)
			if err != nil {
				return nil, nil, err
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
		martifact, err := hartifact.ProtoArtifactToManifest(artifact.Hashout, artifact.Artifact)
		if err != nil {
			return nil, nil, err
		}

		m.Artifacts = append(m.Artifacts, martifact)
	}

	manifestArtifact, err := hartifact.NewManifestArtifact(cachedir, m)
	if err != nil {
		return nil, nil, err
	}

	cacheArtifacts = append(cacheArtifacts, ExecuteResultArtifact{
		Artifact: manifestArtifact,
	})

	return cacheArtifacts, &m, nil
}

func keyRefOutputs(ref *pluginv1.TargetRef, outputs []string) string {
	if len(outputs) == 0 {
		outputs = nil
	} else {
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
	refstr := tref.Format(def.GetRef())
	_ = refstr

	dirfs := hfs.At(e.Cache, def.GetRef().GetPackage(), e.targetDirName(def.GetRef()), hashin)

	manifest, err := hartifact.ManifestFromFS(dirfs)
	if err != nil {
		return nil, false, fmt.Errorf("ManifestFromFS: %w", err)
	}

	var artifacts []hartifact.ManifestArtifact
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

package engine

import (
	"context"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/hephbuild/hephv2/internal/hartifact"
	"github.com/hephbuild/hephv2/internal/hcore/hlog"
	"github.com/hephbuild/hephv2/internal/hfs"
	"github.com/hephbuild/hephv2/internal/hlocks"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"github.com/zeebo/xxh3"
	"google.golang.org/protobuf/proto"
	"io"
	"os"
	"time"
)

func (e *Engine) hashout(ctx context.Context, artifact *pluginv1.Artifact) (string, error) {
	h := xxh3.New()

	b, err := proto.Marshal(artifact)
	if err != nil {
		return "", err
	}

	_, err = h.Write(b)
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

func (e *Engine) CacheLocally(ctx context.Context, def *LightLinkedTarget, hashin string, sandboxArtifacts []ExecuteResultOutput) ([]ExecuteResultOutput, error) {
	// TODO: locks

	cachedir := hfs.At(e.Cache, def.Ref.Package, "__"+def.Ref.Name, hashin)

	cacheArtifacts := make([]ExecuteResultOutput, 0, len(sandboxArtifacts))

	for _, artifact := range sandboxArtifacts {
		scheme, rest, err := hartifact.ParseUri(artifact.Uri)
		if err != nil {
			return nil, err
		}

		var prefix string
		switch artifact.Type {
		case pluginv1.Artifact_TYPE_OUTPUT:
			prefix = "out_"
		case pluginv1.Artifact_TYPE_LOG:
			prefix = "log_"
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

		cacheArtifacts = append(cacheArtifacts, ExecuteResultOutput{
			Hashout:  hashout,
			Artifact: cachedArtifact,
		})
	}

	manifestfs := hfs.At(cachedir, ArtifactManifestName)

	m := Manifest{
		Version:   "v1",
		CreatedAt: time.Now(),
		Hashin:    hashin,
	}
	for _, artifact := range cacheArtifacts {
		m.Artifacts = append(m.Artifacts, ManifestArtifact{
			Hashout:  artifact.Hashout,
			Group:    artifact.Group,
			Name:     artifact.Name,
			Type:     artifact.Type,
			Encoding: artifact.Encoding,
		})
	}

	b, err := json.Marshal(m)
	if err != nil {
		return nil, err
	}

	err = hfs.WriteFile(manifestfs, "", b, os.ModePerm)
	if err != nil {
		return nil, err
	}

	cacheArtifacts = append(cacheArtifacts, ExecuteResultOutput{
		Artifact: manifestV1Artifact(cachedir),
	})

	return cacheArtifacts, nil
}

func manifestV1Artifact(fs hfs.OS) *pluginv1.Artifact {
	return &pluginv1.Artifact{
		Name:     ArtifactManifestName,
		Type:     pluginv1.Artifact_TYPE_MANIFEST_V1,
		Encoding: pluginv1.Artifact_ENCODING_NONE,
		Uri:      "file://" + hfs.At(fs, ArtifactManifestName).Path(),
	}
}

func (e *Engine) ResultFromLocalCache(ctx context.Context, def *LightLinkedTarget, outputs []string, hashin string) (*ExecuteResult, bool, error) {
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
	dirfs := hfs.At(e.Cache, def.Ref.Package, "__"+def.Ref.Name, hashin)

	{
		l := hlocks.NewFlock2(dirfs, "", ArtifactManifestName, false)
		err := l.RLock(ctx)
		if err != nil {
			return nil, false, err
		}
		locks.Add(l.RUnlock)
	}

	mainfestb, err := hfs.ReadFile(dirfs, ArtifactManifestName)
	if err != nil {
		return nil, false, err
	}

	var manifest Manifest
	err = json.Unmarshal(mainfestb, &manifest)
	if err != nil {
		return nil, false, err
	}

	var artifacts []ManifestArtifact
	for _, output := range outputs {
		outputArtifacts := manifest.GetArtifacts(output)

		artifacts = append(artifacts, outputArtifacts...)
	}

	for _, artifact := range artifacts {
		l := hlocks.NewFlock2(dirfs, "", artifact.Name, false)
		err := l.RLock(ctx)
		if err != nil {
			return nil, false, err
		}
		locks.Add(l.RUnlock)
	}

	execOutputs := make([]ExecuteResultOutput, 0, len(artifacts))
	for _, artifact := range artifacts {
		execOutputs = append(execOutputs, ExecuteResultOutput{
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

	execOutputs = append(execOutputs, ExecuteResultOutput{
		Artifact: manifestV1Artifact(dirfs),
	})

	return &ExecuteResult{
		Hashin:  manifest.Hashin,
		Outputs: execOutputs,
	}, true, nil
}

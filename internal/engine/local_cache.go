package engine

import (
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	"hash"
	"io"
	"iter"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hproto/hashpb"
	"github.com/hephbuild/heph/lib/pluginsdk"

	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hfs"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/zeebo/xxh3"
)

var ErrLocalCacheNotFound = errors.New("artifact not found")

type LocalCache interface {
	Reader(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (io.ReadCloser, error)
	Exists(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (bool, error)
	Writer(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (io.WriteCloser, error)
	Delete(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) error
	ListArtifacts(ctx context.Context, ref *pluginv1.TargetRef, hashin string) iter.Seq2[string, error]
	ListVersions(ctx context.Context, ref *pluginv1.TargetRef) iter.Seq2[string, error]
}

func (e *Engine) hashout(ctx context.Context, ref *pluginv1.TargetRef, artifact pluginsdk.Artifact) (string, error) {
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

func (e *Engine) cacheLocally(
	ctx context.Context,
	def *LightLinkedTarget,
	hashin string,
	sandboxArtifacts []*ExecuteArtifact,
) ([]*ResultArtifact, error) {
	step, ctx := hstep.New(ctx, "Caching...")
	defer step.Done()

	cacheArtifacts := make([]*ResultArtifact, 0, len(sandboxArtifacts))

	for _, artifact := range sandboxArtifacts {
		contentType, err := artifact.GetContentType()
		if err != nil {
			return nil, err
		}

		contentSize, err := artifact.GetContentSize()
		if err != nil {
			return nil, err
		}

		src, err := artifact.GetContentReader()
		if err != nil {
			return nil, err
		}
		defer src.Close()

		cacheArtifact, err := e.cacheArtifactLocally(ctx, def.GetRef(), hashin, CacheLocallyArtifact{
			Reader:      src,
			Size:        contentSize,
			Type:        artifact.GetType(),
			Group:       artifact.GetGroup(),
			Name:        artifact.GetName(),
			ContentType: hartifact.ManifestArtifactContentType(contentType),
		}, artifact.Hashout)
		if err != nil {
			return nil, fmt.Errorf("%q/%q: %w", artifact.GetGroup(), artifact.GetName(), err)
		}

		cacheArtifacts = append(cacheArtifacts, cacheArtifact)

		_ = src.Close()
	}

	return cacheArtifacts, nil
}

func (e *Engine) createLocalCacheManifest(ctx context.Context, ref *pluginv1.TargetRef, hashin string, artifacts []*ResultArtifact) (*hartifact.Manifest, error) {
	m := &hartifact.Manifest{
		Version:   "v1",
		Target:    tref.Format(ref),
		CreatedAt: time.Now(),
		Hashin:    hashin,
	}
	for _, artifact := range artifacts {
		martifact, err := hartifact.ProtoArtifactToManifest(artifact.GetHashout(), artifact.Artifact)
		if err != nil {
			return nil, err
		}

		m.Artifacts = append(m.Artifacts, martifact)
	}

	w, err := e.CacheSmall.Writer(ctx, ref, hashin, hartifact.ManifestName)
	if err != nil {
		return nil, err
	}
	defer w.Close()

	err = hartifact.EncodeManifest(w, m)
	if err != nil {
		return nil, err
	}

	err = w.Close()
	if err != nil {
		return nil, err
	}

	return m, nil
}

type cachePluginArtifact struct {
	artifact hartifact.ManifestArtifact
	ref      *pluginv1.TargetRef
	hashin   string
	cache    LocalCache
}

func (e cachePluginArtifact) GetGroup() string {
	return e.artifact.Group
}
func (e cachePluginArtifact) GetName() string {
	return e.artifact.Name
}
func (e cachePluginArtifact) GetType() pluginv1.Artifact_Type {
	return pluginv1.Artifact_Type(e.artifact.Type)
}

func (e cachePluginArtifact) GetContentReader() (io.ReadCloser, error) {
	return e.cache.Reader(context.TODO(), e.ref, e.hashin, e.artifact.Name)
}

func (e cachePluginArtifact) GetContentSize() (int64, error) {
	return e.artifact.Size, nil
}

func (e cachePluginArtifact) GetContentType() (pluginsdk.ArtifactContentType, error) {
	switch e.artifact.ContentType {
	case hartifact.ManifestArtifactContentTypeTar:
		return pluginsdk.ArtifactContentTypeTar, nil
	case hartifact.ManifestArtifactContentTypeTarGz:
		return pluginsdk.ArtifactContentTypeTarGz, nil
	}

	return "", fmt.Errorf("invalid artifact content type: %q", e.artifact.ContentType)
}

func (e *Engine) ClearCacheLocally(
	ctx context.Context,
	ref *pluginv1.TargetRef,
	hashin string,
) error {
	step, ctx := hstep.New(ctx, "Clearing...")
	defer step.Done()

	if err := e.CacheLarge.Delete(ctx, ref, hashin, ""); err != nil {
		return fmt.Errorf("fs: %w", err)
	}

	if err := e.CacheSmall.Delete(ctx, ref, hashin, ""); err != nil {
		return fmt.Errorf("db: %w", err)
	}

	return nil
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

func (e *Engine) ResultFromLocalCache(ctx context.Context, def *LightLinkedTarget, outputs []string, hashin string) (*Result, bool, error) {
	ctx, span := tracer.Start(ctx, "ResultFromLocalCache")
	defer span.End()

	res, ok, err := e.resultFromLocalCacheInner(ctx, def, outputs, hashin)
	if err != nil {
		if errors.Is(err, hfs.ErrNotExist) {
			return nil, false, nil
		}
		return nil, false, err
	}

	return res, ok, nil
}

func (e *Engine) readAnyCache(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (io.ReadCloser, error) {
	for _, c := range [...]LocalCache{e.CacheSmall, e.CacheLarge} {
		r, err := c.Reader(ctx, ref, hashin, name)
		if err != nil {
			if errors.Is(err, ErrLocalCacheNotFound) {
				continue
			}

			return nil, err
		}

		return r, nil
	}

	return nil, ErrLocalCacheNotFound
}

func (e *Engine) existsAnyCache(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (bool, LocalCache, error) {
	for _, c := range [...]LocalCache{e.CacheSmall, e.CacheLarge} {
		exists, err := c.Exists(ctx, ref, hashin, name)
		if err != nil {
			return false, c, err
		}

		if exists {
			return true, c, nil
		}
	}

	return false, nil, nil
}

func (e *Engine) resultFromLocalCacheInner(ctx context.Context, def *LightLinkedTarget, outputs []string, hashin string) (*Result, bool, error) {
	r, err := e.readAnyCache(ctx, def.GetRef(), hashin, hartifact.ManifestName)
	if err != nil {
		if errors.Is(err, ErrLocalCacheNotFound) {
			return nil, false, nil
		}

		return nil, false, err
	}
	defer r.Close()

	m, err := hartifact.DecodeManifest(r)
	if err != nil {
		return nil, false, err
	}

	artifacts := make([]hartifact.ManifestArtifact, 0, len(outputs))
	for _, output := range outputs {
		outputArtifacts := m.GetArtifacts(output)

		artifacts = append(artifacts, outputArtifacts...)
	}

	execArtifacts := make([]*ResultArtifact, 0, len(artifacts))
	for _, artifact := range artifacts {
		exists, cache, err := e.existsAnyCache(ctx, def.GetRef(), hashin, artifact.Name)
		if err != nil {
			return nil, false, err
		}

		if !exists {
			return nil, false, nil
		}

		execArtifacts = append(execArtifacts, &ResultArtifact{
			Artifact: cachePluginArtifact{
				artifact: artifact,
				ref:      def.GetRef(),
				hashin:   hashin,
				cache:    cache,
			},
			Manifest: artifact,
		})
	}

	return Result{
		Def:       def,
		Hashin:    m.Hashin,
		Artifacts: execArtifacts,
		Manifest:  m,
	}.Sorted(), true, nil
}

type CacheLocallyArtifact struct {
	Reader      io.Reader
	Size        int64
	Type        pluginv1.Artifact_Type
	Group       string
	Name        string
	ContentType hartifact.ManifestArtifactContentType
}

func (e *Engine) cacheArtifactLocally(ctx context.Context, ref *pluginv1.TargetRef, hashin string, art CacheLocallyArtifact, hashout string) (*ResultArtifact, error) {
	cache := e.CacheSmall
	if art.Size > 100_000 { // 100kb
		cache = e.CacheLarge
	}

	var prefix string
	switch art.Type {
	case pluginv1.Artifact_TYPE_OUTPUT:
		prefix = "out_"
	case pluginv1.Artifact_TYPE_SUPPORT_FILE:
		prefix = "support_"
	case pluginv1.Artifact_TYPE_LOG:
		prefix = "log_"
	case pluginv1.Artifact_TYPE_OUTPUT_LIST_V1, pluginv1.Artifact_TYPE_UNSPECIFIED:
		fallthrough
	default:
		return nil, fmt.Errorf("invalid artifact type: %s", art.Type)
	}

	dst, err := cache.Writer(ctx, ref, hashin, prefix+art.Name)
	if err != nil {
		return nil, err
	}
	defer dst.Close()

	_, err = io.Copy(dst, art.Reader)
	if err != nil {
		return nil, fmt.Errorf("copy to local: %w", err)
	}

	err = dst.Close()
	if err != nil {
		return nil, err
	}

	manifestArtifact := hartifact.ManifestArtifact{
		Hashout:     hashout,
		Group:       art.Group,
		Name:        prefix + art.Name,
		Size:        art.Size,
		Type:        hartifact.ManifestArtifactType(art.Type),
		ContentType: art.ContentType,
	}

	return &ResultArtifact{
		Artifact: cachePluginArtifact{
			artifact: manifestArtifact,
			ref:      ref,
			hashin:   hashin,
			cache:    cache,
		},
		Manifest: manifestArtifact,
	}, nil
}

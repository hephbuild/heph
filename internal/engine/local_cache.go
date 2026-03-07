package engine

import (
	"archive/tar"
	"bytes"
	"context"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"hash"
	"io"
	"iter"
	"os"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hproto/hashpb"
	"github.com/hephbuild/heph/lib/pluginsdk"

	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/htar"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hfs"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/zeebo/xxh3"
)

var LocalCacheNotFoundError = errors.New("artifact not found")

type LocalCache interface {
	Reader(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (io.ReadCloser, error)
	Exists(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (bool, error)
	Writer(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (io.WriteCloser, error)
	Delete(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) error
	ListArtifacts(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) iter.Seq2[string, error]
	ListVersions(ctx context.Context, ref *pluginv1.TargetRef, name string) iter.Seq2[string, error]
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

func (e *Engine) CacheLocally(
	ctx context.Context,
	def *LightLinkedTarget,
	hashin string,
	sandboxArtifacts []*ResultArtifact,
) ([]*ResultArtifact, *hartifact.Manifest, error) {
	step, ctx := hstep.New(ctx, "Caching...")
	defer step.Done()

	cacheArtifacts := make([]*ResultArtifact, 0, len(sandboxArtifacts))

	for _, artifact := range sandboxArtifacts {
		if artifact.GetType() == pluginv1.Artifact_TYPE_MANIFEST_V1 {
			continue
		}

		src, err := e.contentReaderNormalizer(artifact)
		if err != nil {
			return nil, nil, err
		}
		defer src.Close()

		cacheArtifact, err := e.cacheLocally(ctx, def.GetRef(), hashin, CacheLocallyArtifact{
			Reader: src,
			Size:   0,
			Type:   artifact.GetType(),
			Group:  artifact.GetGroup(),
			Name:   artifact.GetName(),
		}, artifact.Hashout)
		if err != nil {
			return nil, nil, err
		}

		cacheArtifacts = append(cacheArtifacts, cacheArtifact)
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

	cacheArtifacts = append(cacheArtifacts, &ResultArtifact{
		Artifact: &manifestPluginArtifact{manifest: m},
	})

	return cacheArtifacts, &m, nil
}

type cachePluginArtifact struct {
	group, name string
	type_       pluginv1.Artifact_Type
	cache       LocalCache
}

func (e cachePluginArtifact) GetGroup() string {
	return e.group
}
func (e cachePluginArtifact) GetName() string {
	return e.name
}
func (e cachePluginArtifact) GetType() pluginv1.Artifact_Type {
	return e.type_
}

func (e cachePluginArtifact) GetProto() *pluginv1.Artifact {
	panic("TO REMOVE")
}

func (e cachePluginArtifact) GetContentReader() (io.ReadCloser, error) {
	return hartifact.Reader(e)
}

func (e cachePluginArtifact) GetContentSize() (int64, error) {
	return hartifact.Size(e)
}

type manifestPluginArtifact struct {
	manifest hartifact.Manifest

	manifestBytes     []byte
	manifestBytesOnce sync.Once
}

func (e *manifestPluginArtifact) GetGroup() string {
	return ""
}
func (e *manifestPluginArtifact) GetName() string {
	return hartifact.ManifestName
}
func (e *manifestPluginArtifact) GetType() pluginv1.Artifact_Type {
	return pluginv1.Artifact_TYPE_MANIFEST_V1
}

func (e *manifestPluginArtifact) GetProto() *pluginv1.Artifact {
	panic("TO REMOVE")
}

func (e *manifestPluginArtifact) marshal() {
	e.manifestBytesOnce.Do(func() {
		b, err := json.Marshal(e.manifest)
		if err != nil {
			panic(err)
		}

		e.manifestBytes = b
	})
}

func (e *manifestPluginArtifact) GetContentReader() (io.ReadCloser, error) {
	e.marshal()

	return io.NopCloser(bytes.NewReader(e.manifestBytes)), nil
}

func (e *manifestPluginArtifact) GetContentSize() (int64, error) {
	e.marshal()

	return int64(len(e.manifestBytes)), nil
}

func (e *Engine) contentReaderNormalizer(artifact pluginsdk.Artifact) (io.ReadCloser, error) {
	partifact := artifact.GetProto()

	if partifact.HasFile() {
		content := partifact.GetFile()

		pr, pw := io.Pipe()

		p := htar.NewPacker(pw)

		sourcefs := hfs.NewOS(content.GetSourcePath())

		go func() {
			f, err := hfs.Open(sourcefs)
			if err != nil {
				_ = pr.CloseWithError(err)

				return
			}
			defer f.Close()

			err = p.WriteFile(f, content.GetOutPath())
			if err != nil {
				_ = pr.CloseWithError(err)

				return
			}

			_ = f.Close()
			_ = pw.Close()
		}()

		return pr, nil
	}

	if partifact.HasRaw() {
		content := partifact.GetRaw()

		pr, pw := io.Pipe()

		p := htar.NewPacker(pw)

		mode := int64(os.ModePerm)
		if content.GetX() {
			mode |= 0111 // executable
		}

		go func() {
			err := p.Write(bytes.NewReader(content.GetData()), &tar.Header{
				Typeflag: tar.TypeReg,
				Name:     content.GetPath(),
				Size:     int64(len(content.GetData())),
				Mode:     mode,
			})
			if err != nil {
				_ = pr.CloseWithError(err)

				return
			}
		}()

		_ = pw.Close()

		return pr, nil
	}

	return artifact.GetContentReader()
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

func (e *Engine) ResultFromLocalCache(ctx context.Context, def *LightLinkedTarget, outputs []string, hashin string) (*ExecuteResult, bool, error) {
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
	for _, c := range []LocalCache{e.CacheSmall, e.CacheLarge} {
		r, err := c.Reader(ctx, ref, hashin, name)
		if err != nil {
			if errors.Is(err, LocalCacheNotFoundError) {
				continue
			}

			return nil, err
		}

		return r, nil
	}

	return nil, LocalCacheNotFoundError
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

func (e *Engine) resultFromLocalCacheInner(ctx context.Context, def *LightLinkedTarget, outputs []string, hashin string) (*ExecuteResult, bool, error) {
	r, err := e.readAnyCache(ctx, def.GetRef(), hashin, hartifact.ManifestName)
	if err != nil {
		if errors.Is(err, LocalCacheNotFoundError) {
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
			Hashout: artifact.Hashout,
			Artifact: cachePluginArtifact{
				group: artifact.Group,
				name:  artifact.Name,
				type_: pluginv1.Artifact_Type(artifact.Type),
				cache: cache,
			},
		})
	}

	execArtifacts = append(execArtifacts, &ResultArtifact{
		Artifact: &manifestPluginArtifact{manifest: m},
	})

	return ExecuteResult{
		Def:       def,
		Hashin:    m.Hashin,
		Artifacts: execArtifacts,
	}.Sorted(), true, nil
}

type CacheLocallyArtifact struct {
	Reader io.Reader
	Size   int64
	Type   pluginv1.Artifact_Type
	Group  string
	Name   string
}

func (e *Engine) cacheLocally(ctx context.Context, ref *pluginv1.TargetRef, hashin string, art CacheLocallyArtifact, hashout string) (*ResultArtifact, error) {
	cache := e.CacheSmall
	if art.Size > 100_000 {
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
	case pluginv1.Artifact_TYPE_OUTPUT_LIST_V1, pluginv1.Artifact_TYPE_MANIFEST_V1, pluginv1.Artifact_TYPE_UNSPECIFIED:
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
		return nil, err
	}

	err = dst.Close()
	if err != nil {
		return nil, err
	}

	return &ResultArtifact{
		Hashout: hashout,
		Artifact: cachePluginArtifact{
			group: art.Group,
			name:  prefix + art.Name,
			type_: art.Type,
			cache: cache,
		},
	}, nil
}

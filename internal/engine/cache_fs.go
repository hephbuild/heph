package engine

import (
	"context"
	"encoding/hex"
	"errors"
	"io"
	"iter"

	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hproto/hashpb"
	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/zeebo/xxh3"
)

type FSCache struct {
	root hfs.OS // absolute cache root
}

func (c FSCache) Exists(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (bool, error) {
	_, err := c.path(ref, hashin, name).Lstat()
	if err != nil {
		if errors.Is(err, hfs.ErrNotExist) {
			return false, nil
		}

		return false, err
	}

	return true, nil
}

var _ LocalCache = (*FSCache)(nil)

func NewFSCache(root hfs.OS) *FSCache {
	return &FSCache{root: root}
}

func (c FSCache) targetDirName(ref *pluginv1.TargetRef) string {
	if len(ref.GetArgs()) == 0 {
		return "__" + ref.GetName()
	}

	h := xxh3.New()
	hashpb.Hash(h, ref, tref.OmitHashPb)

	return "__" + ref.GetName() + "_" + hex.EncodeToString(h.Sum(nil))
}

func (c FSCache) path(ref *pluginv1.TargetRef, hashin, name string) hfs.Node {
	return c.root.At(ref.GetPackage(), c.targetDirName(ref), hashin)
}

func (c FSCache) Reader(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (io.ReadCloser, error) {
	return hfs.Open(c.path(ref, hashin, name))
}

func (c FSCache) Writer(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) (io.WriteCloser, error) {
	return hfs.Create(c.path(ref, hashin, name))
}

func (c FSCache) Delete(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) error {
	return c.path(ref, hashin, name).RemoveAll()
}

func (c FSCache) ListArtifacts(ctx context.Context, ref *pluginv1.TargetRef, hashin, name string) iter.Seq2[string, error] {
	return func(yield func(string, error) bool) {
		entries, err := c.path(ref, hashin, "").ReadDir()
		if err != nil {
			yield("", err)
			return
		}

		for _, entry := range entries {
			if entry.IsDir() {
				continue
			}

			if !yield(entry.Name(), nil) {
				return
			}
		}
	}
}

func (c FSCache) ListVersions(ctx context.Context, ref *pluginv1.TargetRef, name string) iter.Seq2[string, error] {
	return func(yield func(string, error) bool) {
		entries, err := c.path(ref, "", "").ReadDir()
		if err != nil {
			yield("", err)
			return
		}

		for _, entry := range entries {
			if !entry.IsDir() {
				continue
			}

			if !yield(entry.Name(), nil) {
				return
			}
		}
	}
}

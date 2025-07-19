package remotecache

import (
	"context"
	"errors"
	"io"

	"cloud.google.com/go/storage"
	"github.com/hephbuild/heph/lib/pluginsdk"
)

const DriverNameGCS = "gcs"

func NewGCS(ctx context.Context, bucketName string) (*GCS, error) {
	client, err := storage.NewClient(ctx)
	if err != nil {
		return nil, err
	}

	bucket := client.Bucket(bucketName)

	return &GCS{bucket: bucket}, nil
}

var _ pluginsdk.Cache = (*GCS)(nil)
var _ pluginsdk.CacheHas = (*GCS)(nil)

type GCS struct {
	bucket *storage.BucketHandle
}

func (g GCS) Has(ctx context.Context, key string) (bool, error) {
	obj := g.bucket.Object(key)
	_, err := obj.Attrs(ctx)
	if err != nil {
		if errors.Is(err, storage.ErrObjectNotExist) {
			return false, nil
		}

		return false, err
	}

	return true, nil
}

func (g GCS) Store(ctx context.Context, key string, r io.Reader) error {
	obj := g.bucket.Object(key)

	w := obj.NewWriter(ctx)
	defer w.Close()

	_, err := io.Copy(w, r)
	if err != nil {
		return err
	}

	return w.Close()
}

func (g GCS) Get(ctx context.Context, key string) (io.ReadCloser, error) {
	obj := g.bucket.Object(key)

	r, err := obj.NewReader(ctx)
	if err != nil {
		if errors.Is(err, storage.ErrObjectNotExist) {
			return nil, pluginsdk.ErrCacheNotFound
		}

		return nil, err
	}

	return r, nil
}

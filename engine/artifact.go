package engine

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/engine/artifacts"
	"github.com/hephbuild/heph/engine/observability"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/flock"
	"github.com/hephbuild/heph/utils/fs"
	"github.com/hephbuild/heph/utils/hio"
	"io"
	"os"
	"path/filepath"
)

func UncompressedPathFromArtifact(ctx context.Context, target *Target, artifact artifacts.Artifact, dir string) (string, error) {
	uncompressedPath := filepath.Join(dir, artifact.FileName())
	if fs.PathExists(uncompressedPath) {
		return uncompressedPath, nil
	}

	if artifact.Compressible() {
		gzPath := filepath.Join(dir, artifact.GzFileName())
		if fs.PathExists(gzPath) {
			l := flock.NewFlock("", gzPath+".lock")
			err := l.Lock(ctx)
			if err != nil {
				return "", err
			}

			defer func() {
				err := l.Unlock()
				if err != nil {
					log.Errorf("unlock: %v: %v", gzPath, err)
				}
			}()

			if fs.PathExists(uncompressedPath) {
				return uncompressedPath, nil
			}

			observability.Status(ctx, TargetOutputStatus(target, artifact.Name(), "Decompressing..."))

			log.Debugf("ungz %v to %v", gzPath, uncompressedPath)

			tmpp := fs.ProcessUniquePath(uncompressedPath)

			tf, err := os.Create(tmpp)
			if err != nil {
				return "", err
			}
			defer tf.Close()

			gr, err := artifacts.UncompressedReaderFromArtifact(artifact, dir)
			if err != nil {
				return "", fmt.Errorf("ungz: reader: %w", err)
			}
			defer gr.Close()

			grc, cancel := hio.ContextReader(ctx, gr)
			defer cancel()

			_, err = io.Copy(tf, grc)
			if err != nil {
				return "", fmt.Errorf("ungz: cp: %w", err)
			}

			_ = grc.Close()
			_ = tf.Close()

			err = os.Rename(tmpp, uncompressedPath)
			if err != nil {
				return "", err
			}

			return uncompressedPath, nil
		}
	}

	return "", fmt.Errorf("%v: artifact not found", artifact.Name())
}

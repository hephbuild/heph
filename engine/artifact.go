package engine

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/locks"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/utils/xio"
	"io"
	"os"
	"path/filepath"
)

func UncompressedPathFromArtifact(ctx context.Context, target *Target, artifact artifacts.Artifact, dir string) (string, error) {
	uncompressedPath := filepath.Join(dir, artifact.FileName())
	if xfs.PathExists(uncompressedPath) {
		return uncompressedPath, nil
	}

	if artifact.Compressible() {
		gzPath := filepath.Join(dir, artifact.GzFileName())
		if xfs.PathExists(gzPath) {
			l := locks.NewFlock("", gzPath+".lock")
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

			if xfs.PathExists(uncompressedPath) {
				return uncompressedPath, nil
			}

			status.Emit(ctx, tgt.TargetOutputStatus(target, artifact.Name(), "Decompressing..."))

			log.Debugf("ungz %v to %v", gzPath, uncompressedPath)

			tmpp := xfs.ProcessUniquePath(uncompressedPath)

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

			grc, cancel := xio.ContextReader(ctx, gr)
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

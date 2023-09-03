package lcache

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/locks"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/utils/xio"
	"github.com/hephbuild/heph/utils/xmath"
	"io"
	"math"
	"os"
	"path/filepath"
)

func UncompressedPathFromArtifact(ctx context.Context, target graph.Targeter, artifact artifacts.Artifact, dir string, size int64) (string, error) {
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

			cancel := xio.ContextCloser(ctx, gr)
			defer cancel()

			if size > 0 && status.IsInteractive(ctx) {
				_, err = xio.Copy(tf, gr, func(written int64) {
					percent := math.Round(xmath.Percent(written, size))

					status.EmitInteractive(ctx, tgt.TargetOutputStatus(target, artifact.Name(), xmath.FormatPercent("Decompressing [P]...", percent)))
				})
			} else {
				_, err = io.Copy(tf, gr)
			}

			if err != nil {
				return "", fmt.Errorf("ungz: cp: %w", err)
			}

			_ = gr.Close()
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

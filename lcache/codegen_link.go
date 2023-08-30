package lcache

import (
	"context"
	"errors"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/tar"
	"github.com/hephbuild/heph/utils/xfs"
	"os"
)

func (e *LocalCacheState) codegenLink(ctx context.Context, target *Target) error {
	if target.Codegen == "" {
		return nil
	}

	status.Emit(ctx, tgt.TargetStatus(target, "Linking output..."))

	for name, paths := range target.Out.Named() {
		if err := ctx.Err(); err != nil {
			return err
		}

		switch target.Codegen {
		case specs.CodegenCopy, specs.CodegenCopyNoExclude:
			tarf, err := e.UncompressedReaderFromArtifact(target.Artifacts.OutTar(name), target)
			if err != nil {
				return err
			}

			err = tar.UntarContext(ctx, tarf, e.Root.Root.Abs(), tar.UntarOptions{})
			_ = tarf.Close()
			if err != nil {
				return err
			}
		case specs.CodegenLink:
			for _, path := range paths {
				from := path.WithRoot(target.OutExpansionRoot().Abs()).Abs()
				to := path.WithRoot(e.Root.Root.Abs()).Abs()

				info, err := os.Lstat(to)
				if err != nil && !errors.Is(err, os.ErrNotExist) {
					return err
				}
				exists := err == nil

				if exists {
					isLink := info.Mode().Type() == os.ModeSymlink

					if !isLink {
						log.Warnf("linking codegen: %v already exists", to)
						continue
					}

					err := os.Remove(to)
					if err != nil {
						return err
					}
				}

				err = xfs.CreateParentDir(to)
				if err != nil {
					return err
				}

				err = os.Symlink(from, to)
				if err != nil {
					return err
				}
			}
		}
	}

	return nil
}

func (e *LocalCacheState) IsCodegenLink(path string) bool {
	info, err := os.Lstat(path)
	if err != nil {
		return false
	}

	isLink := info.Mode().Type() == os.ModeSymlink

	if !isLink {
		return false
	}

	target, err := os.Readlink(path)
	if err != nil {
		log.Debugf("IsCodegenLink: Readlink: %v", err)
		return false
	}

	match, err := xfs.PathMatchAny(target, e.Path.Abs()+"/**/*")
	if err != nil {
		log.Debugf("IsCodegenLink: PathMatchAny: %v", err)
		return false
	}

	return match
}

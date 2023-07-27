package rcache

import (
	"context"
	"fmt"
	"github.com/c2fo/vfs/v6"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/instance"
	"github.com/hephbuild/heph/utils/xio"
	"os"
	"path"
)

func (e *RemoteCache) remoteCacheLocation(loc vfs.Location, ttarget graph.Targeter) (vfs.Location, error) {
	target := ttarget.GraphTarget()

	// TODO: cache
	inputHash, err := e.LocalCache.HashInput(target)
	if err != nil {
		return nil, err
	}

	return loc.NewLocation(path.Join(target.Package.Path, target.Name, inputHash) + "/")
}

func (e *RemoteCache) vfsCopyFileIfNotExists(ctx context.Context, from, to vfs.Location, path string, atomic bool) (bool, error) {
	tof, err := to.NewFile(path)
	if err != nil {
		return false, err
	}
	defer tof.Close()

	fromf, err := from.NewFile(path)
	if err != nil {
		return false, err
	}
	defer fromf.Close()

	exists, err := tof.Exists()
	if err != nil {
		return false, err
	}

	if exists {
		tos, _ := tof.Size()
		froms, _ := fromf.Size()
		if tos == froms {
			log.Tracef("vfs copy %v to %v: exists", from.URI(), to.URI())
			return false, nil
		}
	}

	_ = tof.Close()
	_ = fromf.Close()

	err = e.vfsCopyFile(ctx, from, to, path, atomic)
	if err != nil {
		return false, err
	}

	return true, nil
}

func (e *RemoteCache) vfsCopyFile(ctx context.Context, from, to vfs.Location, path string, atomic bool) error {
	log.Tracef("vfs copy %v to %v", from.URI(), to.URI())

	doneTrace := log.TraceTimingDone(fmt.Sprintf("vfs copy to %v", to.URI()))
	defer doneTrace()

	sf, err := from.NewFile(path)
	if err != nil {
		return fmt.Errorf("NewFile sf: %w", err)
	}
	defer sf.Close()

	doneCloser := xio.CloserContext(sf, ctx)
	defer doneCloser()

	ok, err := sf.Exists()
	if err != nil {
		return fmt.Errorf("Exists: %w", err)
	}

	log.Tracef("%v exists: %v", sf.URI(), ok)

	if !ok {
		return fmt.Errorf("copy %v: %w", sf.URI(), os.ErrNotExist)
	}

	if atomic {
		dftmp, err := to.NewFile(path + "_tmp_" + instance.UID)
		if err != nil {
			return fmt.Errorf("NewFile df: %w", err)
		}
		defer dftmp.Close()

		err = sf.CopyToFile(dftmp)
		if err != nil {
			return err
		}

		df, err := to.NewFile(path)
		if err != nil {
			return fmt.Errorf("NewFile df: %w", err)
		}
		defer df.Close()

		err = dftmp.MoveToFile(df)
		if err != nil {
			return fmt.Errorf("Move: %w", err)
		}
	} else {
		df, err := to.NewFile(path)
		if err != nil {
			return fmt.Errorf("NewFile df: %w", err)
		}
		defer df.Close()

		err = sf.CopyToFile(df)
		if err != nil {
			return err
		}
	}

	return nil
}

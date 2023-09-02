package rcache

import (
	vfs "github.com/c2fo/vfs/v6"
	"github.com/c2fo/vfs/v6/backend"
	vfsos "github.com/c2fo/vfs/v6/backend/os"
	"github.com/c2fo/vfs/v6/utils"
	"github.com/hephbuild/heph/utils/xio"
	"github.com/hephbuild/heph/utils/xmath"
	"math"
)

func CopyWithProgress(from, to vfs.File, progress func(percent float64)) error {
	if progress == nil {
		return from.CopyToFile(to)
	}

	sourceSize, _ := from.Size()
	if sourceSize <= 0 {
		progress(-1)

		return from.CopyToFile(to)
	}

	// Should leverage native copy between same filesystem, at the expense of progress
	if from.Location().FileSystem().Scheme() == to.Location().FileSystem().Scheme() &&
		from.Location().FileSystem().Scheme() != vfsos.Scheme {

		progress(-1)

		return from.CopyToFile(to)
	}

	return copyWithTap(from, to, sourceSize, progress)
}

func copyWithTap(from, to vfs.File, sourceSize uint64, progress func(percent float64)) error {
	progress(-1)

	if err := backend.ValidateCopySeekPosition(from); err != nil {
		return err
	}

	buf := make([]byte, utils.TouchCopyMinBufferSize)
	_, err := xio.CopyBuffer(to, from, buf, func(written int64) {
		progress(math.Round(xmath.Percent(written, sourceSize)))
	})
	if err != nil {
		return err
	}

	err = to.Close()
	if err != nil {
		return err
	}

	err = from.Close()
	if err != nil {
		return err
	}

	return nil
}

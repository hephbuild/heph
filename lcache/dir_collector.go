package lcache

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/xfs"
	"io/fs"
	"path/filepath"
)

type ActualFileCollectorDir struct {
	Dir string
}

func (c ActualFileCollectorDir) PopulateActualFiles(ctx context.Context, target *Target, outputs []string) error {
	target.actualOutFiles = &ActualOutNamedPaths{}
	target.actualSupportFiles = make(xfs.RelPaths, 0)

	var err error

	target.actualOutFiles, err = c.collectNamedOut(target.Target, target.Out, c.Dir, outputs)
	if err != nil {
		return fmt.Errorf("out: %w", err)
	}

	if target.HasSupportFiles {
		target.actualSupportFiles, err = c.collectOut(target.Target, target.OutWithSupport.Name(specs.SupportFilesOutput), c.Dir)
		if err != nil {
			return fmt.Errorf("support: %w", err)
		}
	}

	return nil
}

func (c ActualFileCollectorDir) collectNamedOut(target *graph.Target, namedPaths *graph.OutNamedPaths, root string, outputs []string) (*ActualOutNamedPaths, error) {
	tp := &ActualOutNamedPaths{}

	for name, paths := range namedPaths.Named() {
		if !ads.Contains(outputs, name) {
			continue
		}

		tp.ProvisionName(name)

		files, err := c.collectOut(target, paths, root)
		if err != nil {
			return nil, err
		}

		tp.AddAll(name, files)
	}

	tp.Sort()

	return tp, nil
}

func (c ActualFileCollectorDir) collectOut(target *graph.Target, files xfs.RelPaths, root string) (xfs.RelPaths, error) {
	outSet := sets.NewSet(func(p xfs.RelPath) string {
		return p.RelRoot()
	}, len(files))

	for _, file := range files {
		pattern := file.RelRoot()

		if !xfs.IsGlob(pattern) && !xfs.PathExists(filepath.Join(root, pattern)) {
			return nil, fmt.Errorf("%v did not output %v", target.Addr, pattern)
		}

		err := xfs.StarWalk(root, pattern, nil, func(path string, d fs.DirEntry, err error) error {
			outSet.Add(xfs.NewRelPath(path))

			return nil
		})
		if err != nil {
			return nil, fmt.Errorf("collect output %v: %w", file.RelRoot(), err)
		}
	}

	out := xfs.RelPaths(outSet.Slice())
	out.Sort()

	return out, nil
}

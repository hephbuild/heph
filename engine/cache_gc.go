package engine

import (
	"errors"
	log "heph/hlog"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"
)

func (e *Engine) GCTargets(targets []*Target, flog func(string, ...interface{}), dryrun bool) error {
	targetDirs := make([]string, 0)
	for _, target := range targets {
		targetDirs = append(targetDirs, e.cacheDirForHash(target, "").Abs())
	}

	return e.runGc(targetDirs, flog, dryrun)
}

func (e *Engine) gcCollectTargetDirs(root string) ([]string, error) {
	targetDirs := make([]string, 0)

	err := filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}

		if !d.IsDir() {
			return err
		}

		if strings.HasPrefix(d.Name(), "__target_") {
			targetDirs = append(targetDirs, path)
		}

		return nil
	})

	return targetDirs, err
}

func (e *Engine) runGc(targetDirs []string, flog func(string, ...interface{}), dryrun bool) error {
	if flog == nil {
		flog = func(string, ...interface{}) {}
	}

	type gcEntry struct {
		Time     time.Time
		HashPath string
		Latest   bool
	}

	targetHashDirs := map[string]*Target{}
	for _, target := range e.Targets.Slice() {
		if !target.Cache.Enabled {
			continue
		}

		targetHashDirs[e.cacheDirForHash(target, "").Abs()] = target
	}

	for _, dir := range targetDirs {
		reldir, _ := filepath.Rel(e.HomeDir.Abs(), dir)

		flog("%v:", reldir)
		target, ok := targetHashDirs[dir]
		if !ok {
			flog("Not part of schema or not cached, delete")
			if !dryrun {
				err := os.RemoveAll(dir)
				if err != nil {
					log.Error(err)
				}
			}

			continue
		}

		dirEntries, err := os.ReadDir(dir)
		if err != nil {
			if errors.Is(err, fs.ErrNotExist) {
				continue
			}

			return err
		}

		var latestTarget string
		for _, entry := range dirEntries {
			if entry.Name() == "latest" {
				latestTarget, _ = os.Readlink(filepath.Join(dir, entry.Name()))
			}
		}

		entries := make([]gcEntry, 0)
		for _, entry := range dirEntries {
			if entry.Name() == "latest" {
				continue
			}

			p := filepath.Join(dir, entry.Name())

			info, _ := os.Lstat(p)
			t := info.ModTime()

			entries = append(entries, gcEntry{
				Time:     t,
				HashPath: p,
				Latest:   latestTarget == p,
			})
		}

		if len(entries) == 0 {
			flog("Nothing left, delete")
			if !dryrun {
				err := os.RemoveAll(dir)
				if err != nil {
					log.Error(err)
				}
			}

			continue
		}

		// Sort fresher first
		sort.SliceStable(entries, func(i, j int) bool {
			if entries[i].Latest {
				return true
			}

			return entries[i].Time.Unix() > entries[j].Time.Unix()
		})

		targetKeep := target.Cache.History
		flog("keep %v", targetKeep)

		elog := func(entry gcEntry, keep bool) {
			actionStr := "Delete"
			if keep {
				actionStr = "Keep  "
			}

			latestStr := ""
			if entry.Latest {
				latestStr = "latest"
			}

			flog("* %2s %v %v %v", actionStr, filepath.Base(entry.HashPath), entry.Time.Format(time.RFC3339), latestStr)
		}

		for i, entry := range entries {
			if i < targetKeep {
				elog(entry, true)

				continue
			}

			elog(entry, false)

			if !dryrun {
				err := os.RemoveAll(entry.HashPath)
				if err != nil {
					log.Error(err)
				}
			}
		}
		flog("")
	}

	return nil
}

func (e *Engine) GC(flog func(string, ...interface{}), dryrun bool) error {
	err := e.gcLock.Lock()
	if err != nil {
		return err
	}

	defer func() {
		err := e.gcLock.Unlock()
		if err != nil {
			log.Errorf("gc: %v", err)
		}
	}()

	targetDirs, err := e.gcCollectTargetDirs(e.HomeDir.Join("cache").Abs())
	if err != nil {
		return err
	}

	return e.runGc(targetDirs, flog, dryrun)
}

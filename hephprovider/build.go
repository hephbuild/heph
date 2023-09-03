package hephprovider

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/utils/locks"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/utils/xsync"
	"go.uber.org/multierr"
	"os"
	"os/exec"
	"path/filepath"
	"sync"
)

func build(srcDir, goos, goarch, out string) error {
	log.Infof("Building heph %v/%v...", goos, goarch)

	err := xfs.CreateParentDir(out)
	if err != nil {
		return err
	}

	cmd := exec.Command("go", "build", "-o", out, "github.com/hephbuild/heph/cmd/heph")
	cmd.Dir = srcDir
	cmd.Env = os.Environ()
	cmd.Env = append(cmd.Env, []string{
		"GOOS=" + goos,
		"GOARCH=" + goarch,
		"CGO_ENABLED=0",
	}...)

	b, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("%v/%v: %v:\n%s", goos, goarch, err, b)
	}

	return nil
}

var matrix = [][2]string{
	{"linux", "amd64"},
	{"linux", "arm64"},
	{"darwin", "amd64"},
	{"darwin", "arm64"},
}

func hephBinName(goos, goarch string) string {
	return fmt.Sprintf("heph_%v_%v", goos, goarch)
}

func key(goos, goarch string) string {
	return goos + "/" + goarch
}

type buildMatrix = maps.OMap[string, string]

var buildOnce = xsync.Once[*buildMatrix]{}

func buildAll(ctx context.Context, srcDir, outDir string) (*buildMatrix, error) {
	status.Emit(ctx, status.String("Building heph..."))

	return buildOnce.Do(func() (*buildMatrix, error) {
		return doBuildAll(ctx, srcDir, outDir)
	})
}

func doBuildAll(ctx context.Context, srcDir, outDir string) (*buildMatrix, error) {
	log.Debugf("building heph: src:%v out:%v matrix: %v", srcDir, outDir, matrix)

	l := locks.NewFlock("", filepath.Join(outDir, "lock"))
	err := l.Lock(ctx)
	if err != nil {
		return nil, err
	}

	defer l.Unlock()

	err = os.MkdirAll(outDir, os.ModePerm)
	if err != nil {
		return nil, err
	}

	var wg sync.WaitGroup
	m := &buildMatrix{}
	errCh := make(chan error)

	for _, e := range matrix {
		goos := e[0]
		goarch := e[1]

		wg.Add(1)
		go func() {
			defer wg.Done()
			out := filepath.Join(outDir, hephBinName(goos, goarch))

			err := build(srcDir, goos, goarch, out)
			if err != nil {
				errCh <- fmt.Errorf("build: %v/%v: %w", goos, goarch, err)
			}

			m.Set(key(goos, goarch), out)
		}()
	}

	go func() {
		wg.Wait()
		close(errCh)
	}()

	for berr := range errCh {
		err = multierr.Append(err, berr)
	}

	log.Tracef("building heph: done: %v", m.Raw())

	return m, err
}

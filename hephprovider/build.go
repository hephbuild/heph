package hephprovider

import (
	"fmt"
	"go.uber.org/multierr"
	log "heph/hlog"
	"heph/utils"
	"heph/utils/fs"
	"heph/utils/maps"
	"os"
	"os/exec"
	"path/filepath"
	"sync"
)

func build(srcDir, goos, goarch, out string) error {
	log.Infof("Building heph %v/%v...", goos, goarch)

	err := fs.CreateParentDir(out)
	if err != nil {
		return err
	}

	cmd := exec.Command("go", "build", "-o", out, "heph/cmd/heph")
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

var buildOnce = utils.Once[*maps.Map[string, string]]{}

func buildAll(srcDir, outDir string) (*maps.Map[string, string], error) {
	return buildOnce.Do(func() (*maps.Map[string, string], error) {
		return doBuildAll(srcDir, outDir)
	})
}

func doBuildAll(srcDir, outDir string) (*maps.Map[string, string], error) {
	log.Debugf("building heph: src:%v out:%v matrix: %v", srcDir, outDir, matrix)

	err := os.MkdirAll(outDir, os.ModePerm)
	if err != nil {
		return nil, err
	}

	var wg sync.WaitGroup
	m := &maps.Map[string, string]{}
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

	log.Tracef("building heph: done: %v", m.Map())

	return m, err
}

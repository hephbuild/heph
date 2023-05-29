package lwl

import (
	"bufio"
	"bytes"
	"fmt"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/xfs"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
)

var lddLib = regexp.MustCompile(`\s*(\S+) => (\S+).*`)
var lddNoArrowLib = regexp.MustCompile(`\s*(\S+).*`)

func parseLdd(b []byte) ([]string, error) {
	if bytes.Contains(b, []byte("statically linked")) {
		return nil, nil
	}

	scan := bufio.NewScanner(bytes.NewBuffer(b))
	scan.Split(bufio.ScanLines)

	var libs []string

	for scan.Scan() {
		line := scan.Text()

		groups := lddLib.FindStringSubmatch(line)
		if len(groups) == 3 {
			libs = append(libs, groups[2])
		} else {
			groups := lddNoArrowLib.FindStringSubmatch(line)

			if len(groups) == 2 {
				path := groups[1]
				if filepath.IsAbs(path) {
					libs = append(libs, path)
				}
			}
		}
	}

	return libs, nil
}

func Analyze(binPath string) ([]string, error) {
	cmd := exec.Command("ldd", binPath)
	lddb, err := cmd.Output()
	if err != nil {
		return nil, fmt.Errorf("%v: %w: %s", binPath, err, bytes.TrimSpace(lddb))
	}

	libPaths, err := parseLdd(lddb)
	if err != nil {
		return nil, err
	}

	return libPaths, nil
}

func expandPaths(paths []string) ([]string, error) {
	expPaths := make([]string, 0, len(paths))

	for _, path := range paths {
		info, err := os.Stat(path)
		if err != nil {
			return nil, err
		}

		if info.IsDir() {
			entries, err := os.ReadDir(path)
			if err != nil {
				return nil, err
			}

			for _, entry := range entries {
				if entry.IsDir() {
					continue
				}

				expPaths = append(expPaths, filepath.Join(path, entry.Name()))
			}
		} else {
			expPaths = append(expPaths, path)
		}
	}

	return expPaths, nil
}

func ExtractLibs(binPath string, extras []string, libPath, ldPath string) error {
	binLibs, err := Analyze(binPath)
	if err != nil {
		return err
	}
	allLibs := sets.NewStringSet(len(binLibs))
	allLibs.AddAll(binLibs)

	extras, err = expandPaths(extras)
	if err != nil {
		return err
	}

	for _, path := range extras {
		libPaths, err := Analyze(path)
		if err != nil {
			return err
		}

		allLibs.AddAll(libPaths)
	}

	err = os.MkdirAll(libPath, os.ModePerm)
	if err != nil {
		return err
	}

	err = xfs.CreateParentDir(ldPath)
	if err != nil {
		return err
	}

	foundld := false
	for _, path := range allLibs.Slice() {
		name := filepath.Base(path)
		path, err = filepath.EvalSymlinks(path)
		if err != nil {
			return err
		}

		if strings.Contains(name, "ld-linux") {
			foundld = true
			err := xfs.Cp(path, ldPath)
			if err != nil {
				return err
			}
		} else {
			err := xfs.Cp(path, filepath.Join(libPath, name))
			if err != nil {
				return err
			}
		}
	}

	if !foundld {
		return fmt.Errorf("did not find ld")
	}

	return nil
}

package lwl

import (
	"bufio"
	"bytes"
	"fmt"
	"heph/utils/sets"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
)

type PackLibrary struct {
	Path string
}

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
		return nil, fmt.Errorf("%w: %s", err, lddb)
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

func Pack(binPath string, extras []string) (Bin, error) {
	var err error

	binLibs, err := Analyze(binPath)
	if err != nil {
		return Bin{}, err
	}
	allLibs := sets.NewStringSet(len(binLibs))
	allLibs.AddAll(binLibs)

	extras, err = expandPaths(extras)
	if err != nil {
		return Bin{}, err
	}

	for _, path := range extras {
		libPaths, err := Analyze(path)
		if err != nil {
			return Bin{}, err
		}

		allLibs.AddAll(libPaths)
	}

	libs := make([]Lib, 0, allLibs.Len())
	for _, path := range allLibs.Slice() {
		b, err := os.ReadFile(path)
		if err != nil {
			return Bin{}, err
		}

		libs = append(libs, Lib{
			Path:    path,
			Content: b,
		})
	}

	var bin Bin
	bin.Bin, err = os.ReadFile(binPath)
	if err != nil {
		return Bin{}, err
	}
	bin.Libs = libs

	return bin, nil
}

package lib

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

func RmCache() error {
	fmt.Println("Removing cache")
	cache, err := CachePath()
	if err != nil {
		return err
	}

	return os.RemoveAll(cache)
}

func TargetCacheRoot(tgt string, elems ...string) (string, error) {
	args := []string{"query", "cacheroot", tgt}
	args = append(args, defaultOpts.Args()...)

	cmd := exec.Command("heph", args...)

	b, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("%v: %v", err, string(b))
	}

	return filepath.Join(pathAppend(strings.TrimSpace(string(b)), elems...)...), nil
}

func TargetCacheInputHash(tgt string) (string, error) {
	p, err := TargetCacheRoot(tgt, "hash_input")
	if err != nil {
		return "", err
	}

	return FileContent(p)
}

func TargetCacheOutputHash(tgt, output string) (string, error) {
	p, err := TargetCacheRoot(tgt, "hash_out_"+output)
	if err != nil {
		return "", err
	}

	return FileContent(p)
}

func ValidateCache(tgt string, outputs []string) error {
	root, err := TargetCacheRoot(tgt)
	if err != nil {
		return err
	}

	if !PathExists(root) {
		return fmt.Errorf("%v doesnt exist", root)
	}

	expectedFilesMap := map[string]struct{}{}

	expectedFiles := []string{
		filepath.Join(root, "_output"),
		filepath.Join(root, "_output_hash"),
		filepath.Join(root, "hash_input"),
		filepath.Join(root, "version"),
	}
	for _, output := range outputs {
		expectedFiles = append(expectedFiles, filepath.Join(root, "hash_out_"+output))
		expectedFiles = append(expectedFiles, filepath.Join(root, "out_.tar.gz"+output))
		expectedFiles = append(expectedFiles, filepath.Join(root, "out_.tar.gz.list"+output))
	}

	for _, p := range expectedFiles {
		expectedFilesMap[p] = struct{}{}

		if !PathExists(p) {
			return fmt.Errorf("%v doesnt exist", p)
		}
	}

	actualFiles, err := os.ReadDir(root)
	if err != nil {
		return err
	}
	for _, file := range actualFiles {
		p := filepath.Join(root, file.Name())
		if _, ok := expectedFilesMap[p]; !ok {
			return fmt.Errorf("%v exists, but shouldn't, expected: %v", p, expectedFiles)
		}
	}

	return nil
}

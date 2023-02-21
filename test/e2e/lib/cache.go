package lib

import (
	"fmt"
	"os"
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
	cmd := command("query", "cacheroot", tgt)

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

func ValidateCache(tgt string, outputs []string, fromRemote bool, expectLogs bool) error {
	expected := []string{
		"hash_input",
	}
	if expectLogs {
		expected = append(expected, "log.txt")
	}
	if len(outputs) > 0 {
		expected = append(expected, "_output")
		expected = append(expected, "_output_hash")
	}
	for _, output := range outputs {
		expected = append(expected, "hash_out_"+output)
		expected = append(expected, fmt.Sprintf("out_%v.tar.gz", output))
		expected = append(expected, fmt.Sprintf("out_%v.tar.gz.list", output))
	}

	root, err := TargetCacheRoot(tgt)
	if err != nil {
		return err
	}

	fmt.Println(tgt+" cache folder:", root)

	return validateFolderContent(root, expected)
}

func validateFolderContent(root string, expected []string) error {
	if !PathExists(root) {
		return fmt.Errorf("%v doesnt exist", root)
	}

	for i, p := range expected {
		expected[i] = filepath.Join(root, p)
	}

	expectedMap := map[string]struct{}{}
	for _, p := range expected {
		expectedMap[p] = struct{}{}

		if !PathExists(p) {
			return fmt.Errorf("%v doesnt exist", p)
		}
	}

	actual, err := os.ReadDir(root)
	if err != nil {
		return err
	}
	for _, file := range actual {
		p := filepath.Join(root, file.Name())
		if _, ok := expectedMap[p]; !ok {
			return fmt.Errorf("%v exists, but shouldn't, expected: %v", p, expected)
		}
	}

	return nil
}

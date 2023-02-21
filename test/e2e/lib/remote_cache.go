package lib

import (
	"fmt"
	"path/filepath"
)

func ValidateRemoteCache(root, tgt string, outputs []string, expectLogs bool) error {
	fmt.Println("remote cache folder:", root)

	expected := []string{
		"hash_input",
		"manifest.json",
	}
	if expectLogs {
		expected = append(expected, "log.tar.gz")
	}
	for _, output := range outputs {
		expected = append(expected, "hash_out_"+output)
		expected = append(expected, "out_.tar.gz"+output)
	}

	tp, err := ParseTargetPath(tgt)
	if err != nil {
		return err
	}

	hash, err := TargetCacheInputHash(tgt)
	if err != nil {
		return err
	}

	tgtroot := filepath.Join(root, tp.Package, tp.Name, hash)

	return validateFolderContent(tgtroot, expected)
}

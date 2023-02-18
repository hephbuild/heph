package lib

import (
	"fmt"
	"path/filepath"
)

func ValidateRemoteCache(root, tgt string, outputs []string) error {
	fmt.Println("remote cache folder:", root)

	expected := []string{
		"hash_input",
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

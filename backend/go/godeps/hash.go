package main

import (
	"crypto/sha1"
	"encoding/hex"
	"io"
	"os"
	"path/filepath"
)

func hashString(s string) string {
	return hashBytes([]byte(s))
}

func hashBytes(b []byte) string {
	h := sha1.New()
	h.Write(b)
	return hex.EncodeToString(h.Sum(nil))
}

func hashFile(path string) string {
	f, err := os.Open(path)
	if err != nil {
		return ""
	}
	defer f.Close()

	h := sha1.New()
	_, _ = io.Copy(h, f)
	return hex.EncodeToString(h.Sum(nil))
}

func findGoModRoot(root string) string {
	dir := filepath.Join(root, Env.Package)

	for {
		_, err := os.Stat(filepath.Join(dir, "go.mod"))
		if err == nil {
			return dir
		}

		if dir == Env.Root {
			panic("go.mod not found")
		}

		dir = filepath.Dir(dir)
		if dir == "" {
			panic("go.mod not found")
		}
	}

	return ""
}

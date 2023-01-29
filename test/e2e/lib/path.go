package lib

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

func pathAppend(p string, elems ...string) []string {
	return append([]string{p}, elems...)
}

var rootCache string

func cachedRootPath() (string, error) {
	if rootCache != "" {
		return rootCache, nil
	}

	var buf bytes.Buffer
	cmd := command("query", "root")
	cmd.Stderr = &buf

	b, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("%v: %s", err, buf.String())
	}
	rootCache = strings.TrimSpace(string(b))

	return rootCache, err
}

type TargetPath struct {
	Package string
	Name    string
	Output  string
}

func ParseTargetPath(s string) (TargetPath, error) {
	var buf bytes.Buffer
	cmd := command("query", "parsetarget", s)
	cmd.Stderr = &buf

	b, err := cmd.Output()
	if err != nil {
		return TargetPath{}, fmt.Errorf("%v: %s", err, buf.String())
	}

	var tp TargetPath
	err = json.Unmarshal(b, &tp)
	if err != nil {
		return TargetPath{}, err
	}

	return tp, nil
}

func RootPath(elems ...string) (string, error) {
	root, err := cachedRootPath()
	if err != nil {
		return "", err
	}

	return filepath.Join(pathAppend(root, elems...)...), err
}

func HomePath(elems ...string) (string, error) {
	return RootPath(pathAppend(".heph", elems...)...)
}

func CachePath(elems ...string) (string, error) {
	return HomePath(pathAppend("cache", elems...)...)
}

func PathExists(filename string) bool {
	_, err := os.Lstat(filename)
	return err == nil
}

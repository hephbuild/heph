package lib

import (
	"bytes"
	"os"
	"strings"
	"time"
)

func FileContent(p string) (string, error) {
	b, err := os.ReadFile(p)
	if err != nil {
		return "", err
	}

	return strings.TrimSpace(string(b)), nil
}

func FileModTime(p string) (string, error) {
	i, err := os.Stat(p)
	if err != nil {
		return "", err
	}

	return i.ModTime().Format(time.RFC3339Nano), nil
}

func WriteFile(p, content string) error {
	return os.WriteFile(p, []byte(content), os.ModePerm)
}

func ReWriteFile(p, content string) error {
	_, err := FileModTime(p)
	if err != nil {
		return err
	}

	return os.WriteFile(p, []byte(content), os.ModePerm)
}

func ReplaceFile(p, old, new string) error {
	b, err := os.ReadFile(p)
	if err != nil {
		return err
	}

	nb := bytes.ReplaceAll(b, []byte(old), []byte(new))

	return os.WriteFile(p, nb, os.ModePerm)
}

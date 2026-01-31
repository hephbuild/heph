package hephprovider

import (
	"testing"
)

func TestGetDownloadURL(t *testing.T) {
	tests := []struct {
		version string
		os      string
		arch    string
		want    string
	}{
		{"1.2.3", "linux", "amd64", "https://github.com/hephbuild/heph-artifacts-v0/releases/download/1.2.3/heph_linux_amd64"},
	}

	for _, tt := range tests {
		t.Run(tt.version, func(t *testing.T) {
			got := GetDownloadURL(tt.version, tt.os, tt.arch)
			if got != tt.want {
				t.Errorf("GetDownloadURL() = %v, want %v", got, tt.want)
			}
		})
	}
}
func TestDownload(t *testing.T) {
	t.SkipNow()

	// 13252ae has heph_linux_arm64 asset
	// https://github.com/hephbuild/heph-artifacts-v0/releases/download/13252ae/heph_linux_arm64
	version := "13252ae"
	goos := "linux"
	goarch := "arm64"

	dir := t.TempDir()

	path, err := Download(dir, "heph", version, goos, goarch)
	if err != nil {
		t.Fatal(err)
	}

	if path == "" {
		t.Fatal("path is empty")
	}

	t.Logf("Downloaded to %v", path)
}

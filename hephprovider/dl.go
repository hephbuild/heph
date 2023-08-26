package hephprovider

import (
	"fmt"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/xfs"
	"io"
	"net/http"
	"os"
	"path/filepath"
)

const BaseUrl = "https://storage.googleapis.com/heph-build"

func Download(dir, binName, version, goos, goarch string) (string, error) {
	err := os.MkdirAll(dir, os.ModePerm)
	if err != nil {
		return "", err
	}

	if binName == "" {
		binName = hephBinName(goos, goarch)
	}
	dstPath := filepath.Join(dir, binName)

	if xfs.PathExists(dstPath) {
		log.Debugf("%v already exists", dstPath)

		return dstPath, nil
	}

	log.Infof("Downloading heph %v %v/%v", version, goos, goarch)

	url := fmt.Sprintf("%v/%v/heph_%v_%v", BaseUrl, version, goos, goarch)

	res, err := http.Get(url)
	if err != nil {
		return "", err
	}
	defer res.Body.Close()

	if res.StatusCode != http.StatusOK {
		return "", fmt.Errorf("%v: status: %v", url, res.StatusCode)
	}

	dst, err := os.OpenFile(dstPath, os.O_RDWR|os.O_CREATE|os.O_TRUNC, 0744)
	if err != nil {
		return "", err
	}
	defer dst.Close()

	_, err = io.Copy(dst, res.Body)
	if err != nil {
		_ = os.RemoveAll(dstPath)
		return "", err
	}

	err = xfs.CloseEnsureROFD(dst)
	if err != nil {
		return "", err
	}

	return dstPath, nil
}

//go:build !release

package hversion

import (
	"encoding/hex"
	"io"
	"os"
	"sync"

	"github.com/zeebo/xxh3"
)

var versionOnce sync.Once
var versionComputed string

func Version() string {
	versionOnce.Do(func() {
		path, err := os.Executable()
		if err != nil {
			panic(err)
		}

		h := xxh3.New()
		f, err := os.Open(path)
		if err != nil {
			panic(err)
		}
		defer f.Close()

		_, err = io.Copy(h, f)
		if err != nil {
			panic(err)
		}

		versionComputed = "v0.0.0+dev-" + hex.EncodeToString(h.Sum(nil))
	})

	return versionComputed
}

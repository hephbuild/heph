package hinstance

import (
	"encoding/hex"
	"fmt"
	"os"
	"sync"
	"time"

	"github.com/zeebo/xxh3"
)

func gen() string {
	host, _ := os.Hostname()
	return fmt.Sprintf("%v_%v_%v", os.Getpid(), host, time.Now().UnixNano())
}

var UID = gen()

var Hash = sync.OnceValue(func() string {
	p, err := os.Executable()
	if err != nil {
		panic(err)
	}

	b, err := os.ReadFile(p)
	if err != nil {
		panic(err)
	}

	h := xxh3.New()
	_, _ = h.Write(b)

	return hex.EncodeToString(h.Sum(nil))
})

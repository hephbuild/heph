package huuid

import (
	"strconv"
	"sync/atomic"

	"github.com/hephbuild/heph/internal/hinstance"
)

var c atomic.Uint64

func New() string {
	return hinstance.LocalUID + strconv.FormatUint(c.Add(1), 10)
}

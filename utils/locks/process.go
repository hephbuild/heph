package locks

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/hephbuild/heph/utils/xsync"
	"github.com/shirou/gopsutil/v3/process"
	"strconv"
	"time"
)

var processDetails = maps.Map[int, *xsync.Once[string]]{
	Default: func(k int) *xsync.Once[string] {
		return &xsync.Once[string]{}
	},
	Expiration: 5 * time.Second,
}

func getProcessDetails(pid int) string {
	if pid < 0 {
		return strconv.Itoa(pid)
	}

	once := processDetails.Get(pid)

	return once.MustDo(func() (string, error) {
		return computeProcessDetails(pid), nil
	})
}

func computeProcessDetails(pid int) string {
	ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
	defer cancel()

	p, _ := process.NewProcessWithContext(ctx, int32(pid))
	if p != nil {
		cmdLine, _ := p.CmdlineWithContext(ctx)
		if len(cmdLine) > 0 {
			return fmt.Sprintf("%v (%v)", cmdLine, pid)
		}
	}

	return strconv.Itoa(pid)
}

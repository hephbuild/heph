package hlocks

import (
	"context"
	"fmt"
	cache "github.com/Code-Hex/go-generics-cache"
	"github.com/shirou/gopsutil/v3/process"
	"strconv"
	"sync"
	"time"
)

var processDetails = cache.New[int, func() string]()

func getProcessDetails(pid int) string {
	if pid <= 0 {
		return strconv.Itoa(pid)
	}

	do, _ := processDetails.GetOrSet(pid, sync.OnceValue(computeProcessDetails(pid)), cache.WithExpiration(5*time.Second))

	return do()
}

func computeProcessDetails(pid int) func() string {
	return func() string {
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
}

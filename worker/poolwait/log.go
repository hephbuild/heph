package poolwait

import (
	"fmt"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/xtime"
	"github.com/hephbuild/heph/worker"
	"os"
	"time"
)

func logUI(name string, deps *worker.WaitGroup, pool *worker.Pool) error {
	start := time.Now()
	printProgress := func() {
		s := deps.TransitiveCount()
		log.Infof("Progress %v: %v/%v %v", name, s.Done, s.All, xtime.RoundDuration(time.Since(start), 1).String())
	}

	printWorkersStatus := func() {
		for _, w := range pool.Workers {
			j := w.CurrentJob
			if j == nil {
				continue
			}

			duration := time.Since(j.TimeStart)

			if duration < 5*time.Second {
				// Skip printing short jobs
				continue
			}

			status := w.GetStatus().String(log.Renderer())
			if status == "" {
				status = "Waiting..."
			}

			runtime := fmt.Sprintf("%v", xtime.RoundDuration(duration, 1).String())

			fmt.Fprintf(os.Stderr, " %v %v\n", runtime, status)
		}
	}

	t := time.NewTicker(time.Second)
	defer t.Stop()

	c := 1
	for {
		select {
		case <-t.C:
			printProgress()
			if c >= 5 {
				c = 1
				printWorkersStatus()
			}
			c++
			continue
		case <-deps.Done():
			// will break
		}

		printProgress()
		return nil
	}
}

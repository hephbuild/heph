package poolwait

import (
	"fmt"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/xtime"
	"github.com/hephbuild/heph/worker2"
	"os"
	"strings"
	"time"
)

func printWhatItsWaitingOn(dep worker2.Dep, indent string) {
	fmt.Println(indent, dep.GetName(), dep.GetState().String(), ":", len(dep.GetDepsObj().Dependencies()), "deps")
	for _, d := range dep.GetDepsObj().Dependencies() {
		if d.GetState().IsFinal() {
			return
		}

		printWhatItsWaitingOn(d, "  "+indent)
	}
}

func logUI(name string, deps worker2.Dep, pool *worker2.Engine) error {
	start := time.Now()
	printProgress := func() {
		s := worker2.CollectStats(deps)

		extra := ""
		if s.Failed > 0 || s.Skipped > 0 || s.Suspended > 0 {
			extra += " ("
			if s.Failed > 0 {
				extra += fmt.Sprintf("%v failed, ", s.Failed)
			}
			if s.Skipped > 0 {
				extra += fmt.Sprintf("%v skipped, ", s.Skipped)
			}
			if s.Suspended > 0 {
				extra += fmt.Sprintf("%v suspended, ", s.Suspended)
			}
			extra = strings.TrimSuffix(extra, ", ")
			extra += ")"
		}

		log.Infof("Progress %v: %v/%v %v%v", name, s.Completed, s.All, xtime.RoundDuration(time.Since(start), 1).String(), extra)
	}

	printWorkersStatus := func() {
		statusm := map[string]struct{}{}
		for _, job := range pool.GetWorkers() {
			if job.Execution().State != worker2.ExecStateSuspended {
				continue
			}

			duration := time.Since(job.Execution().StartedAt)

			status := job.GetStatus().String(log.Renderer())
			if status == "" {
				status = "Suspended..."
			}

			if _, ok := statusm[status]; ok {
				continue
			}
			statusm[status] = struct{}{}

			runtime := fmt.Sprintf("%v", xtime.RoundDuration(duration, 1).String())

			fmt.Fprintf(os.Stderr, " %v %v\n", runtime, status)
		}
		if len(statusm) > 0 {
			fmt.Fprintf(os.Stderr, "===\n")
		}
		for _, w := range pool.GetWorkers() {
			j := w.Execution()
			if j == nil {
				continue
			}

			duration := time.Since(j.StartedAt)

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
				printWhatItsWaitingOn(deps, "")
			}
			c++
			continue
		case <-deps.Wait():
			// will break
		}

		printProgress()
		return nil
	}
}

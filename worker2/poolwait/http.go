package poolwait

import (
	"fmt"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/worker2"
	"net"
	"net/http"
	"time"
)

func Server(dep worker2.Dep) (func() error, error) {
	l, err := net.Listen("tcp", ":0")
	if err != nil {
		return nil, err
	}

	log.Infof("poolwait server listening at http://%v", l.Addr().String())

	go func() {
		doneCh := make(chan struct{})
		defer close(doneCh)
		go func() {
			for {
				select {
				case <-doneCh:
					return
				case <-time.After(time.Second):
					log.Infof("poolwait server listening at http://%v", l.Addr().String())
				}

			}
		}()

		err := http.Serve(l, http.HandlerFunc(func(rw http.ResponseWriter, r *http.Request) {
			dep.DeepDo(func(dep worker2.Dep) {
				if dep.GetState().IsFinal() {
					return
				}
				fmt.Fprintf(rw, "# %v %v\n", dep.GetName(), dep.GetState())
				debugString := dep.GetExecutionDebugString()
				if debugString != "" {
					fmt.Fprintf(rw, "%v\n\n", debugString)
				}
			})
		}))
		if err != nil {
			log.Error("polwaitserver", err)
		}
	}()

	return l.Close, nil
}

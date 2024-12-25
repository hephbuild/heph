package pluginexec

import (
	"fmt"
	"github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1/pluginv1connect"
	"io"
	"net/http"
	"strings"
	"time"
)

type PipesHandler struct {
	*Plugin
}

const PipesHandlerPath = pluginv1connect.DriverPipeProcedure + "Handler"

func (p PipesHandler) ServeHTTP(rw http.ResponseWriter, req *http.Request) {
	status, err := p.serveHTTP(rw, req)
	if status > 0 {
		rw.WriteHeader(status)
	}
	if err != nil {
		rw.Write([]byte("\n"))
		rw.Write([]byte(err.Error()))
	}
	return
}

func (p PipesHandler) serveHTTP(rw http.ResponseWriter, req *http.Request) (int, error) {
	i := strings.Index(req.URL.Path, PipesHandlerPath)
	if i < 0 {
		return http.StatusBadRequest, fmt.Errorf("invalid path")
	}

	nameid := req.URL.Path[i+len(PipesHandlerPath)+1:]

	name, id, _ := strings.Cut(nameid, "/")

	if name != p.name {
		return http.StatusBadRequest, fmt.Errorf("name did not match plugin: got %v, expected %v", name, p.name)
	}

	pipe, ok := p.getPipe(id)
	if !ok {
		return http.StatusBadRequest, fmt.Errorf("pipe not found: %v", id)
	}

	if pipe.busy.Swap(true) {
		return http.StatusBadRequest, fmt.Errorf("pipe is busy")
	}

	defer pipe.busy.Store(false)

	rw.WriteHeader(http.StatusOK)

	copyingCh := make(chan struct{})
	flushingDoneCh := make(chan struct{})

	waitFlusherDone := func() {
		close(copyingCh)
		<-flushingDoneCh
	}

	if flusher, ok := rw.(http.Flusher); ok {
		go func() {
			defer close(flushingDoneCh)

			t := time.NewTicker(time.Millisecond)
			defer t.Stop()

			for {
				select {
				case <-req.Context().Done():
					return
				case <-copyingCh:
					return
				case <-t.C:
					flusher.Flush()
				}
			}
		}()
	}

	switch req.Method {
	case http.MethodGet:
		_, err := io.Copy(rw, pipe.r)
		waitFlusherDone()
		if err != nil {
			return -1, fmt.Errorf("http -> pipe: %v", err)
		}
	case http.MethodPost:
		w := pipe.w
		defer w.Close()

		_, err := io.Copy(pipe.w, req.Body)
		waitFlusherDone()
		if err != nil {
			return -1, fmt.Errorf("pipe -> http: %v", err)
		}
	}

	if flusher, ok := rw.(http.Flusher); ok {
		flusher.Flush()
	}

	p.removePipe(id)

	return -1, nil
}

func (p *Plugin) getPipe(id string) (*pipe, bool) {
	p.pipesm.RLock()
	pipe, ok := p.pipes[id]
	p.pipesm.RUnlock()

	return pipe, ok
}

func (p *Plugin) removePipe(id string) {
	p.pipesm.Lock()
	defer p.pipesm.Unlock()
	pipe, ok := p.pipes[id]
	if !ok {
		return
	}

	delete(p.pipes, id)

	pipe.w.Close()

	p.housekeepingPipes()
}

func (p *Plugin) housekeepingPipes() {
	for k, v := range p.pipes {
		if !v.busy.Load() && time.Now().After(v.exp) {
			delete(p.pipes, k)
		}
	}
}

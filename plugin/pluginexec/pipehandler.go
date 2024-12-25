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

type writerFlusher struct {
	w  io.Writer
	hf http.Flusher
}

func (w writerFlusher) Write(p []byte) (int, error) {
	n, err := w.w.Write(p)
	w.hf.Flush()

	return n, err
}

func (p PipesHandler) serveHTTP(rw http.ResponseWriter, req *http.Request) (int, error) {
	i := strings.Index(req.URL.Path, PipesHandlerPath)
	if i < 0 {
		return http.StatusBadRequest, fmt.Errorf("invalid path")
	}

	id := req.URL.Path[i+len(PipesHandlerPath)+1:]

	pipe, ok := p.getPipe(id)
	if !ok {
		return http.StatusBadRequest, fmt.Errorf("pipe not found: %v", id)
	}

	if pipe.busy.Swap(true) {
		return http.StatusBadRequest, fmt.Errorf("pipe is busy")
	}

	defer pipe.busy.Store(false)

	rw.WriteHeader(http.StatusOK)

	switch req.Method {
	case http.MethodGet:
		w := io.Writer(rw)
		if flusher, ok := rw.(http.Flusher); ok {
			w = writerFlusher{
				w:  w,
				hf: flusher,
			}
		}

		_, err := io.Copy(w, pipe.r)
		if err != nil {
			return -1, fmt.Errorf("http -> pipe: %v", err)
		}
	case http.MethodPost:
		w := pipe.w
		defer w.Close()

		_, err := io.Copy(pipe.w, req.Body)
		if err != nil {
			return -1, fmt.Errorf("pipe -> http: %v", err)
		}
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

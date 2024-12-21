package pluginsh

import (
	"fmt"
	"github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1/pluginv1connect"
	"io"
	"net/http"
	"strings"
	"sync"
)

type PipesHandler struct {
	*Plugin
}

const PipesHandlerPath = pluginv1connect.DriverPipeProcedure + "Handler"

func (p PipesHandler) ServeHTTP(rw http.ResponseWriter, req *http.Request) {
	i := strings.Index(req.URL.Path, PipesHandlerPath)
	if i < 0 {
		rw.WriteHeader(http.StatusBadRequest)
		rw.Write([]byte("invalid path"))
		return
	}

	id := req.URL.Path[i+len(PipesHandlerPath)+1:]

	pipe, ok := p.getPipe(id)
	if !ok {
		rw.WriteHeader(http.StatusBadRequest)
		rw.Write([]byte(fmt.Sprintf("pipe not found: %v", id)))
		return
	}

	if pipe.busy.Swap(true) {
		rw.WriteHeader(http.StatusBadRequest)
		rw.Write([]byte("pipe is busy"))
		return
	}

	defer pipe.busy.Store(false)

	var wg sync.WaitGroup
	wg.Add(2)
	ch := make(chan error, 2)

	go func() {
		defer wg.Done()

		if req.Method != http.MethodGet {
			return
		}

		_, err := io.Copy(rw, pipe.r)
		if err != nil {
			ch <- fmt.Errorf("http -> pipe: %v", err)
		}
	}()
	go func() {
		defer wg.Done()

		if req.Method != http.MethodPost {
			return
		}

		_, err := io.Copy(pipe.w, req.Body)
		if err != nil {
			ch <- fmt.Errorf("pipe -> http: %v", err)
		}
	}()

	wg.Wait()
	close(ch)

	for err := range ch {
		rw.Write([]byte("\n"))
		rw.Write([]byte(fmt.Sprint(err)))
	}

	p.removePipe(id)
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
}

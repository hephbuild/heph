package lib

import (
	"bytes"
	"encoding/json"
	"fmt"
	"github.com/vektah/gqlparser/v2/ast"
	"github.com/vektah/gqlparser/v2/formatter"
	"github.com/vektah/gqlparser/v2/parser"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"sync"
	"sync/atomic"
	"time"
)

type MockCloud struct {
	spansm    sync.Mutex
	spans     map[string]Span
	logsBytes int64
}

func NewHephcloudServer() (*httptest.Server, *MockCloud) {
	handler := &MockCloud{
		spans: map[string]Span{},
	}

	return httptest.NewServer(handler), handler
}

type Span struct {
	Event      string `json:"event"`
	SpanID     string `json:"span_id"`
	FinalState string `json:"final_state"`
	TargetFQN  string `json:"target_fqn"`
}

func (h *MockCloud) Spans() map[string]Span {
	return h.spans
}

func (h *MockCloud) LogBytes() int64 {
	return h.logsBytes
}

func (h *MockCloud) SpansPerFQN() map[string][]Span {
	spans := map[string][]Span{}
	for _, span := range h.spans {
		spans[span.TargetFQN] = append(spans[span.TargetFQN], span)
	}

	return spans
}

func (h *MockCloud) gqlResponse(w http.ResponseWriter, data interface{}) {
	b, err := json.Marshal(map[string]interface{}{
		"data": data,
	})
	if err != nil {
		panic(err)
	}

	w.Write(b)
}

func (h *MockCloud) handleRegisterFlowInvocation(w http.ResponseWriter, op *ast.OperationDefinition) {
	h.gqlResponse(w, map[string]interface{}{
		"registerFlowInvocation": map[string]interface{}{
			"id": "id123",
			"invocations": map[string]interface{}{
				"edges": []map[string]interface{}{
					{
						"node": map[string]interface{}{
							"id": "id123",
						},
					},
				},
			},
		},
	})
}

func (h *MockCloud) handleEndInvocation(w http.ResponseWriter, op *ast.OperationDefinition) {
	h.gqlResponse(w, map[string]interface{}{
		"endInvocation": map[string]interface{}{
			"id": "id123",
		},
	})
}

func (h *MockCloud) handleIngestSpanLogs(w http.ResponseWriter, op *ast.OperationDefinition, varsj json.RawMessage) {
	var vars struct {
		SpanId string          `json:"spanId"`
		Data   json.RawMessage `json:"bdata"`
	}
	err := json.Unmarshal(varsj, &vars)
	if err != nil {
		panic(err)
	}

	fmt.Printf("SPANLOGS: %v: %v bytes\n", vars.SpanId, len(vars.Data))
	atomic.AddInt64(&h.logsBytes, int64(len(vars.Data)))

	h.gqlResponse(w, map[string]interface{}{
		"ingestSpanLogs": "id123",
	})
}

func (h *MockCloud) handleIngestSpan(w http.ResponseWriter, op *ast.OperationDefinition, varsj json.RawMessage) {
	var vars struct {
		Spans []Span `json:"spans"`
	}
	err := json.Unmarshal(varsj, &vars)
	if err != nil {
		panic(err)
	}

	fmt.Printf("SPANS: %v\n", len(vars.Spans))

	time.Sleep(time.Second)

	h.spansm.Lock()
	defer h.spansm.Unlock()

	spansr := make([]map[string]interface{}, 0, len(vars.Spans))

	for _, span := range vars.Spans {
		h.spans[span.SpanID] = span

		spansr = append(spansr, map[string]interface{}{
			"id":     span.SpanID,
			"spanId": span.SpanID,
		})
	}

	h.gqlResponse(w, map[string]interface{}{
		"ingestSpans": spansr,
	})
}

func (h *MockCloud) handleGQL(w http.ResponseWriter, r *http.Request) {
	b, err := io.ReadAll(r.Body)
	if err != nil {
		panic(err)
	}
	defer r.Body.Close()

	var req struct {
		Query     string          `json:"query"`
		Variables json.RawMessage `json:"variables"`
	}
	err = json.Unmarshal(b, &req)
	if err != nil {
		panic(err)
	}

	q, err := parser.ParseQuery(&ast.Source{Input: req.Query})
	if err != nil {
		panic(err)
	}

	op := q.Operations[0]

	field := op.SelectionSet[0].(*ast.Field)
	switch field.Name {
	case "login":
		h.gqlResponse(w, map[string]interface{}{
			"login": map[string]interface{}{
				"token": "token123",
				"user": map[string]interface{}{
					"id":    "id123",
					"email": "email@email.com",
				},
			},
		})
	case "auth":
		h.gqlResponse(w, map[string]interface{}{
			"auth": map[string]interface{}{
				"actor": map[string]interface{}{
					"__typename": "User",
					"actor_id":   "id123",
					"actor_type": "user",
				},
			},
		})
	case "registerFlowInvocation":
		h.handleRegisterFlowInvocation(w, op)
	case "ingestSpans":
		h.handleIngestSpan(w, op, req.Variables)
	case "ingestSpanLogs":
		h.handleIngestSpanLogs(w, op, req.Variables)
	case "endInvocation":
		h.handleEndInvocation(w, op)
	case "heartbeatInvocation":
		w.Write(nil)
	default:
		f := formatter.NewFormatter(os.Stdout)
		f.FormatQueryDocument(q)

		panic(fmt.Errorf("unknown field %v", field.Name))
	}
}

func (h *MockCloud) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	h.handleGQL(w, r)
}

func CloudLogin() error {
	var stdout, stderr bytes.Buffer
	cmd := commandO(RunOpts{}, "cloud", "login", "dummylogin", "dummypass")
	cmd.Stderr = io.MultiWriter(os.Stderr, &stderr)
	cmd.Stdout = &stdout

	err := cmd.Run()
	if err != nil {
		return fmt.Errorf("%v: %v", err, stderr.String())
	}

	return nil
}

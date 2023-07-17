package hash

import (
	"encoding/json"
	"fmt"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/xfs"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"time"
)

type hasherEntry struct {
	Value interface{}
	Trace []string
}

type hasherRecorder struct {
	file    string
	entries []hasherEntry
	h       Hash
}

func (r *hasherRecorder) String(val string) {
	r.record(val)
	r.h.String(val)
}

func (r *hasherRecorder) I64(val int64) {
	r.record(val)
	r.h.I64(val)
}

func (r *hasherRecorder) UI32(val uint32) {
	r.record(val)
	r.h.UI32(val)
}

func (r *hasherRecorder) Bool(val bool) {
	r.record(val)
	r.h.Bool(val)
}

func (r *hasherRecorder) Sum() string {
	sum := r.h.Sum()

	err := r.dump(sum)
	if err != nil {
		log.Error("HASH", r.file, err)
	}
	return sum
}

func (r *hasherRecorder) Write(p []byte) (n int, err error) {
	// Make sure to store a copy of the bytes
	c := make([]byte, len(p))
	copy(c, p)

	r.record(c)
	return r.h.Write(c)
}

var root string

func init() {
	_, file, _, _ := runtime.Caller(0)
	i := strings.Index(file, "utils/hash")
	root = file[:i]
}

func (r *hasherRecorder) trace(skip int) []string {
	var trace []string

	for i := 1 + skip; i < 999; i++ {
		_, file, no, ok := runtime.Caller(i)
		if !ok {
			break
		}
		file = strings.ReplaceAll(file, root, "")
		trace = append(trace, fmt.Sprintf("%v:%v", file, no))
	}

	return trace
}

func (r *hasherRecorder) record(value interface{}) {
	r.entries = append(r.entries, hasherEntry{
		Trace: r.trace(1),
		Value: value,
	})
}

func (r *hasherRecorder) dump(sum string) error {
	err := xfs.CreateParentDir(r.file)
	if err != nil {
		return err
	}

	filename := r.file
	if xfs.PathExists(filename) {
		b, err := os.ReadFile(filename)
		if err != nil {
			return err
		}

		var fc struct {
			Sum string `json:"sum"`
		}
		err = json.Unmarshal(b, &fc)
		if err != nil {
			return err
		}

		if fc.Sum == sum {
			return nil
		}

		filename = strings.ReplaceAll(filename, ".json", "_"+time.Now().String()+".json")
	}

	f, err := os.Create(filename)
	if err != nil {
		return err
	}

	defer f.Close()

	return json.NewEncoder(f).Encode(map[string]interface{}{
		"sum":     sum,
		"trace":   r.trace(1),
		"entries": r.entries,
	})
}

var debugHashFolder = os.Getenv("HEPH_DEBUG_HASH")

func newHashRecorder(id string) Hash {
	id = strings.ReplaceAll(id, string(os.PathSeparator), "_")

	return &hasherRecorder{
		file: filepath.Join(debugHashFolder, id+".json"),
		h:    NewHash(),
	}
}

func NewDebuggableHash(id func() string) Hash {
	if len(debugHashFolder) > 0 {
		return newHashRecorder(id())
	}

	return NewHash()
}

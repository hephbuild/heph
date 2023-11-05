package tuistatus

import (
	"github.com/hephbuild/heph/status"
)

type handler struct {
	status status.Statuser
}

func (h *handler) Status(status status.Statuser) {
	h.status = status
}

func (h *handler) Interactive() bool {
	return true
}

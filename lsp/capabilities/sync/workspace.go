package sync

import (
	"github.com/hephbuild/heph/lsp/runtime"
	"github.com/tliron/glsp"
	protocol "github.com/tliron/glsp/protocol_3_16"
)

func WorkspaceDidRenameFilesFunc(manager *runtime.Manager) protocol.WorkspaceDidRenameFilesFunc {
	return func(context *glsp.Context, params *protocol.RenameFilesParams) error {
		for _, file := range params.Files {
			manager.RenameDocument(file.OldURI, file.NewURI)
		}

		return nil
	}
}

func WorkspaceDidDeleteFilesFunc(manager *runtime.Manager) protocol.WorkspaceDidDeleteFilesFunc {
	return func(context *glsp.Context, params *protocol.DeleteFilesParams) error {
		for _, file := range params.Files {
			manager.DeleteDocument(file.URI)
		}

		return nil
	}
}

package lifecycle

import (
	"os"

	"github.com/hephbuild/heph/lsp/runtime"
	"github.com/tliron/glsp"
	protocol "github.com/tliron/glsp/protocol_3_16"
)

func InitializeCallback(manager *runtime.Manager, context *glsp.Context, params *protocol.InitializeParams) error {
	root := ""

	if params.RootPath != nil {
		root = *params.RootPath
	}

	if params.RootURI != nil {
		root = *params.RootURI
	}

	if root != "" {
		manager.WorkspaceFolder = root

		return nil
	}

	wkFolders := params.WorkspaceFolders
	if len(wkFolders) >= 1 {
		newVar := wkFolders[0]
		manager.WorkspaceFolder = newVar.URI

		return nil
	}

	v, err := os.Getwd()
	if err != nil {
		return err
	}

	// Fallback to cwd
	manager.WorkspaceFolder = v

	return nil
}

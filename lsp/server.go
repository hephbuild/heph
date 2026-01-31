package lsp

import (
	"errors"
	"sync"

	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/lsp/runtime"
	"github.com/hephbuild/heph/vfssimple"

	"github.com/hephbuild/heph/lsp/capabilities/lang"
	"github.com/hephbuild/heph/lsp/capabilities/lifecycle"
	docsync "github.com/hephbuild/heph/lsp/capabilities/sync"

	tree_sitter "github.com/tree-sitter/go-tree-sitter"
	tree_sitter_python "github.com/tree-sitter/tree-sitter-python/bindings/go"

	"github.com/tliron/commonlog"
	_ "github.com/tliron/commonlog/simple"

	"github.com/tliron/glsp"
	protocol "github.com/tliron/glsp/protocol_3_16"
	"github.com/tliron/glsp/server"
)

var ErrIsClosed = errors.New("server is closed")

type LSPServer interface {
	// Serve blocks until the server stops serving. Errors encountered during this process are returned.
	// Serve will be called only once for the lifetime of an LSPServer
	Serve() error
	Close() error
}

type hephLSP struct {
	protocolHandler *protocol.Handler
	server          *server.Server
	parser          *tree_sitter.Parser

	isClosed bool

	// Force non-copy
	_ [0]sync.Mutex
}

func NewHephServer(root *hroot.State) (LSPServer, error) {
	return newHephLSP(root, false)
}

func (h *hephLSP) Serve() error {
	if h.isClosed {
		return ErrIsClosed
	}

	return h.server.RunStdio()
}

func (h *hephLSP) Close() error {
	h.parser.Close()
	h.server.GetStdio().Close() //nolint
	h.isClosed = true

	return nil
}

func newHephLSP(root *hroot.State, debug bool) (*hephLSP, error) {
	err := configureLogs(root, debug)
	if err != nil {
		return nil, err
	}

	lsp := &hephLSP{}

	parser := tree_sitter.NewParser()
	err = parser.SetLanguage(tree_sitter.NewLanguage(tree_sitter_python.Language()))
	if err != nil {
		return nil, err
	}

	manager, err := runtime.NewManager(parser)
	if err != nil {
		return nil, err
	}

	handler := &protocol.Handler{
		// Lifecycle
		Initialize:  lsp.wrapInitialize(manager),
		Initialized: lsp.wrapInitialized(),
		Shutdown:    lsp.wrapShutdown(),
		SetTrace:    lsp.wrapSetTrace(),

		// Sync
		TextDocumentDidOpen:   docsync.TextDocumentDidOpenWrapper(manager),
		TextDocumentDidChange: docsync.TextDocumentDidChangeFuncWrapper(manager),

		// Lang features
		TextDocumentCompletion: lang.TextDocumentCompletionFuncWrapper(manager),
		TextDocumentHover:      lang.TextDocumentHoverFuncWrapper(manager),

		// Can be implemented Implement code lens to copy addr or give in virtual text a full path of target etc
		// TextDocumentCodeLens:                TextDocumentCodeLensFunc
		TextDocumentReferences:  lang.TextDocumentReferencesFuncWrapper(manager),
		TextDocumentDeclaration: lang.TextDocumentDeclarationFuncWrapper(manager),
		TextDocumentDefinition:  lang.TextDocumentDefinitionFuncWrapper(manager),

		WorkspaceDidRenameFiles: docsync.WorkspaceDidRenameFilesFunc(manager),
		WorkspaceDidDeleteFiles: docsync.WorkspaceDidDeleteFilesFunc(manager),
	}
	server := server.NewServer(handler, runtime.HephLanguage, debug)

	lsp.protocolHandler = handler
	lsp.server = server
	lsp.parser = parser

	return lsp, nil
}

func configureLogs(root *hroot.State, debug bool) error {
	verbosity := 0
	if debug {
		verbosity = 2
	}

	logpath := root.Home.Join(".lsplogs")
	fullpath := logpath.Abs()
	dst, err := vfssimple.NewFile("file://" + fullpath)
	if err != nil {
		return err
	}
	defer dst.Close()

	// tliron/glsp forces us to use this weird logger
	commonlog.Configure(verbosity, &fullpath)

	return nil
}

func (h *hephLSP) wrapInitialize(manager *runtime.Manager) protocol.InitializeFunc {
	return func(context *glsp.Context, params *protocol.InitializeParams) (any, error) {
		// Call lifecycle callback
		err := lifecycle.InitializeCallback(manager, context, params)
		if err != nil {
			return nil, err
		}

		capabilities := h.protocolHandler.CreateServerCapabilities()

		return protocol.InitializeResult{
			Capabilities: capabilities,
			ServerInfo: &protocol.InitializeResultServerInfo{
				Name:    runtime.HephLanguage,
				Version: &runtime.Version,
			},
		}, nil
	}
}

func (h *hephLSP) wrapInitialized() protocol.InitializedFunc {
	return func(context *glsp.Context, params *protocol.InitializedParams) error {
		return nil
	}
}

func (h *hephLSP) wrapShutdown() protocol.ShutdownFunc {
	return func(context *glsp.Context) error {
		protocol.SetTraceValue(protocol.TraceValueOff)
		return nil
	}
}

func (h *hephLSP) wrapSetTrace() protocol.SetTraceFunc {
	return func(context *glsp.Context, params *protocol.SetTraceParams) error {
		protocol.SetTraceValue(params.Value)
		return nil
	}
}

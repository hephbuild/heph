package lang

import (
	"path"
	"strings"

	"github.com/hephbuild/heph/lsp/runtime"
	"github.com/hephbuild/heph/lsp/runtime/document"
	"github.com/hephbuild/heph/lsp/runtime/symbol"
	"github.com/tliron/commonlog"
	"github.com/tliron/glsp"
	protocol "github.com/tliron/glsp/protocol_3_16"
)

var logger = commonlog.GetLogger("lifecycle")

func TextDocumentDeclarationFuncWrapper(manager *runtime.Manager) protocol.TextDocumentDeclarationFunc {
	return func(context *glsp.Context, params *protocol.DeclarationParams) (any, error) {
		if location, found := extractLocation(manager, params.TextDocument.URI, &params.Position); found {
			logger.Noticef("Declaration location: %v", location)
			return location, nil
		}

		return nil, nil
	}
}

func TextDocumentDefinitionFuncWrapper(manager *runtime.Manager) protocol.TextDocumentDefinitionFunc {
	return func(context *glsp.Context, params *protocol.DefinitionParams) (any, error) {
		if location, found := extractLocation(manager, params.TextDocument.URI, &params.Position); found {
			logger.Noticef("Declaration location: %v", location)
			return location, nil
		}

		return nil, nil
	}
}

func extractLocation(manager *runtime.Manager, uri string, pos *protocol.Position) (*protocol.Location, bool) {
	if doc, found := manager.GetDocument(uri); found {
		pos := uint(pos.IndexIn(doc.TextString))

		// If its target address, open from current workspace
		if literal := doc.ExtractCurrentStringLiteral(pos); literal != "" {
			// Check if its a target location
			if strings.HasPrefix(literal, "//") {

				// Fallback to default BUILD file of that directory since we don't know where that heph can be
				literal = strings.Split(literal, ":")[0]
				literal = path.Join(literal, "BUILD")
				fullPath := path.Join(manager.WorkspaceFolder, literal)

				return &protocol.Location{
					URI: addProtocol(fullPath),
					Range: protocol.Range{
						Start: protocol.Position{
							Line:      0,
							Character: 0,
						},
						End: protocol.Position{
							Line:      0,
							Character: 0,
						},
					},
				}, true
			}
		}

		if symbolName := doc.ExtractCurrentSymbolName(pos); symbolName != "" {
			// Current doc
			if sym, found := doc.Query(symbolName); found {
				return buildLocationFromSymbol(doc.FullPath, sym), true
			}

			// Loaded docs
			var loc *protocol.Location
			var symbolFound bool

			// Location symbols that are loaded by "load"
			doc.RangeDocLoads(func(load *document.Load) {
				if !symbolFound {
					doc := load.Doc
					if sym, found := doc.Query(symbolName); found {
						loc = buildLocationFromSymbol(doc.FullPath, sym)
						symbolFound = true
					}
				}
			})

			if symbolFound {
				return loc, true
			}
		}

		if symbolName := doc.ExtractCurrentFunctionName(pos); symbolName != "" {
			// Current doc
			if sym, found := doc.Query(symbolName); found {
				return buildLocationFromSymbol(doc.FullPath, sym), true
			}

			// Loaded docs
			var loc *protocol.Location
			var symbolFound bool
			doc.RangeDocLoads(func(load *document.Load) {
				if !symbolFound {
					doc := load.Doc
					if sym, found := doc.Query(symbolName); found {
						loc = buildLocationFromSymbol(doc.FullPath, sym)
						symbolFound = true
					}
				}
			})

			if symbolFound {
				return loc, true
			}
		}
	}

	return nil, false
}

func buildLocationFromSymbol(uri string, sym *symbol.Symbol) *protocol.Location {
	return &protocol.Location{
		URI: addProtocol(uri),
		Range: protocol.Range{
			Start: protocol.Position{
				Line:      protocol.UInteger(sym.Position.RowStart),
				Character: protocol.UInteger(sym.Position.ColumnStart),
			},
			End: protocol.Position{
				Line:      protocol.UInteger(sym.Position.RowEnd),
				Character: protocol.UInteger(sym.Position.ColumnEnd),
			},
		},
	}
}

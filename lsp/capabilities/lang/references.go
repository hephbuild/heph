package lang

import (
	"github.com/hephbuild/heph/lsp/runtime"
	"github.com/hephbuild/heph/lsp/runtime/document"
	"github.com/tliron/glsp"
	protocol "github.com/tliron/glsp/protocol_3_16"
)

func TextDocumentReferencesFuncWrapper(manager *runtime.Manager) protocol.TextDocumentReferencesFunc {
	return func(context *glsp.Context, params *protocol.ReferenceParams) ([]protocol.Location, error) {
		if doc, found := manager.GetDocument(params.TextDocument.URI); found {
			pos := uint(params.Position.IndexIn(doc.TextString))

			if symbolName := doc.ExtractCurrentSymbolName(pos); symbolName != "" {
				return findReferences(doc, symbolName), nil
			}

			if symbolName := doc.ExtractCurrentFunctionName(pos); symbolName != "" {
				return findReferences(doc, symbolName), nil
			}
		}

		return nil, nil
	}
}

// TODO: We will probably later need a better usage of symbols,
// having a symbol oriented query instead of doc based queries
func findReferences(doc *document.Document, symbolName string) []protocol.Location {
	var locations []protocol.Location

	// Find references in the doc itself
	if calls := doc.QueryCalls(symbolName); len(calls) > 0 {
		for _, sym := range calls {
			loc := protocol.Location{
				URI: addProtocol(doc.FullPath),
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

			locations = append(locations, loc)
		}
	}

	// Search for docs that loads current doc and method
	doc.RangeIsLoadedBy(func(load *document.Load) {
		otherDoc := load.Doc
		if calls := otherDoc.QueryCalls(symbolName); len(calls) > 0 {
			for _, sym := range calls {
				loc := protocol.Location{
					URI: addProtocol(otherDoc.FullPath),
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

				locations = append(locations, loc)
			}
		}
	})

	return locations
}

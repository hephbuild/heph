package lang

import (
	"github.com/hephbuild/heph/lsp/runtime"
	"github.com/hephbuild/heph/lsp/runtime/document"
	"github.com/hephbuild/heph/lsp/runtime/symbol"
	"github.com/tliron/commonlog"
	"github.com/tliron/glsp"
	protocol "github.com/tliron/glsp/protocol_3_16"
)

var Logger = commonlog.GetLogger("completion")

func TextDocumentCompletionFuncWrapper(manager *runtime.Manager) protocol.TextDocumentCompletionFunc {
	return func(context *glsp.Context, params *protocol.CompletionParams) (any, error) {
		var completionItems []protocol.CompletionItem

		if doc, found := manager.GetDocument(params.TextDocument.URI); found {
			byteOffset := params.Position.IndexIn(doc.TextString)

			// If is function we can get args completion
			funName := doc.ExtractCurrentFunctionName(uint(byteOffset))
			if s, found := manager.Query(funName); found {
				if s.Is(symbol.FunctionKind) {
					args := s.Parameters
					if args != nil {
						completionItems = createCompletionItemForArgs(args, s)
					}
				}
			}

			// Append current doc symbols
			for _, s := range doc.Symbols {
				compItem := createCompletionItem(s)
				completionItems = append(completionItems, compItem)
			}

			// Complete symbols that are loaded by "load"
			doc.RangeDocLoads(func(load *document.Load) {
				loadDoc := load.Doc
				if syms := loadDoc.QueryAll(load.Loads); len(syms) > 0 {
					for _, sym := range syms {
						compItem := createCompletionItem(sym)
						completionItems = append(completionItems, compItem)
					}
				}
			})

		}

		// Load Builtin
		allPerKind := manager.BuiltinSymbols
		for _, symbol := range allPerKind {
			compItem := createCompletionItem(symbol)
			completionItems = append(completionItems, compItem)
		}

		return completionItems, nil
	}
}

func createCompletionItem(symbol *symbol.Symbol) protocol.CompletionItem {
	name := symbol.Name
	sig := symbol.Signature
	doc := symbol.DocString
	kind := SymbolKindToCompletionKind(symbol.Kind)

	compItem := protocol.CompletionItem{
		Label:         name,
		InsertText:    &name,
		Kind:          &kind,
		Detail:        &sig,
		Documentation: doc,
	}
	return compItem
}

func createCompletionItemForArgs(args []*symbol.Parameter, s *symbol.Symbol) []protocol.CompletionItem {
	completionItems := []protocol.CompletionItem{}
	for _, arg := range args {
		kind := protocol.CompletionItemKindVariable

		label := arg.Name + "="
		completionItems = append(completionItems, protocol.CompletionItem{
			Label:         label,
			InsertText:    &label,
			Kind:          &kind,
			Documentation: s.DocString,
		})
	}

	return completionItems
}

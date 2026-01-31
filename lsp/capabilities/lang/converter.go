package lang

import (
	"github.com/hephbuild/heph/lsp/runtime/symbol"
	protocol "github.com/tliron/glsp/protocol_3_16"
)

func SymbolKindToCompletionKind(kind symbol.SymbolKind) protocol.CompletionItemKind {
	switch kind {
	case symbol.FunctionKind:
		return protocol.CompletionItemKindFunction

	case symbol.VariableKind:
		return protocol.CompletionItemKindVariable

	case symbol.ClassKind:
		return protocol.CompletionItemKindClass

	default:
		return protocol.CompletionItemKindValue
	}
}

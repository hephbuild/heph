package lang

import (
	"fmt"
	"strings"

	"github.com/hephbuild/heph/lsp/runtime"
	"github.com/hephbuild/heph/lsp/runtime/symbol"
	"github.com/tliron/glsp"
	protocol "github.com/tliron/glsp/protocol_3_16"
)

func TextDocumentHoverFuncWrapper(manager *runtime.Manager) protocol.TextDocumentHoverFunc {
	return func(context *glsp.Context, params *protocol.HoverParams) (*protocol.Hover, error) {
		doc, ok := manager.GetDocument(params.TextDocument.URI)
		if !ok {
			return &protocol.Hover{}, nil
		}

		bytePos := params.Position.IndexIn(doc.TextString)

		if literal := doc.ExtractCurrentStringLiteral(uint(bytePos)); literal != "" {
			return createLiteralHover(literal), nil
		}

		symbolName := doc.ExtractCurrentSymbolName(uint(bytePos))

		// If is an argument inside a function call we can get the function name and args information
		funName := doc.ExtractCurrentFunctionName(uint(bytePos))
		if s, found := manager.Query(funName); found {
			for _, p := range s.Parameters {
				if strings.Contains(p.Name, symbolName) {
					return createArgHover(s, p), nil
				}
			}
		}

		// Query first for current document symbols
		if symbol, found := doc.Query(symbolName); found {
			return createHover(symbol), nil
		}

		if symbol, found := manager.Query(symbolName); found {
			return createHover(symbol), nil
		}

		return nil, nil
	}
}

func createHover(sym *symbol.Symbol) *protocol.Hover {
	var sb strings.Builder

	sb.WriteString(sym.Source)
	sb.WriteString("\n---\n")

	if sym.Is(symbol.VariableKind) {
		varSignature := langDecorateMultiline(sym.Name+" = "+sym.Value, runtime.HephLanguage)
		sb.WriteString(varSignature)
	} else {
		sb.WriteString(langDecorateMultiline(sym.Signature, runtime.HephLanguage))
	}

	sb.WriteString("\n---\n")
	sb.WriteString(sym.DocString)

	return &protocol.Hover{
		Contents: protocol.MarkupContent{
			Kind:  protocol.MarkupKindMarkdown,
			Value: sb.String(),
		},
		Range: &protocol.Range{
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

func createArgHover(fn *symbol.Symbol, param *symbol.Parameter) *protocol.Hover {
	var sb strings.Builder

	sb.WriteString(param.Name)

	if param.Type != "" {
		sb.WriteString(":" + param.Type)
	}

	if param.Value != "" {
		sb.WriteString(" = " + param.Value)
	}

	sb.WriteString(" from " + langDecorate(fn.Name))
	if param.DocString != "" {
		sb.WriteString("\n---\n")
		sb.WriteString(param.DocString)
	}

	return &protocol.Hover{
		Contents: protocol.MarkupContent{
			Kind:  protocol.MarkupKindMarkdown,
			Value: sb.String(),
		},
	}
}

func createLiteralHover(literal string) *protocol.Hover {
	return &protocol.Hover{
		Contents: protocol.MarkupContent{
			Kind:  protocol.MarkupKindMarkdown,
			Value: parseDefinition(literal),
		},
	}
}

func langDecorateMultiline(text, lang string) string {
	return "```" + lang + "\n" + text + "\n```\n"
}

func langDecorate(text string) string {
	return "`" + text + "`"
}

func parseDefinition(literal string) string {
	if ok := strings.HasPrefix(literal, "//"); ok {
		return fmt.Sprintf("Heph Package: %q", literal)
	}

	return literal
}

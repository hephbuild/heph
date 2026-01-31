package query

import (
	"maps"
	"slices"
	"strconv"

	"github.com/hephbuild/heph/lsp/runtime/symbol"
	tree_sitter "github.com/tree-sitter/go-tree-sitter"
)

const callQuery = `
(call function: (identifier) @call.name . (argument_list (_) @call.arg)? ) @call.stmt
`

func QueryCalls(tree *tree_sitter.Tree, text []byte, source string) ([]*symbol.Symbol, error) {
	if tree.RootNode() == nil {
		return nil, ErrEmptyTreeError
	}

	query, err := tree_sitter.NewQuery(lang, callQuery)
	if err != nil {
		return nil, err
	}
	defer query.Close()

	cursor := tree_sitter.NewQueryCursor()
	defer cursor.Close()

	matches := cursor.Matches(query, tree.RootNode(), text)
	funs := map[uintptr]*symbol.Symbol{}

	for match := matches.Next(); match != nil; match = matches.Next() {
		currSymbol := &symbol.Symbol{Kind: symbol.FunctionCallKind, Source: source}
		for _, capture := range match.Captures {
			currentNode := &capture.Node
			patternName := query.CaptureNames()[capture.Index]
			nodeRange := currentNode.Range()
			patternValue := currentNode.Utf8Text(text)

			switch patternName {
			case "call.name":
				// Params query repeats Captures. We use NodeId so we dont need to make multiple queries
				if ss, ok := funs[currentNode.Id()]; ok {
					ss.Parameters = append(ss.Parameters, currSymbol.Parameters...)
					currSymbol = ss
				}

				currSymbol.Name = patternValue
				currSymbol.FullyQualifiedName = patternValue
				currSymbol.Position.RowStart = nodeRange.StartPoint.Row
				currSymbol.Position.ColumnStart = nodeRange.StartPoint.Column

				funs[currentNode.Id()] = currSymbol
			case "call.arg":
				newParam := symbol.Parameter{Name: strconv.Itoa(len(currSymbol.Parameters)), Value: patternValue}
				currSymbol.Parameters = append(currSymbol.Parameters, &newParam)

				// Last pattern
				currSymbol.Position.RowEnd = nodeRange.EndPoint.Row
				currSymbol.Position.ColumnEnd = nodeRange.EndPoint.Column
			case "call.stmt":
				currSymbol.Signature = patternValue
			}
		}
	}

	return slices.Collect(maps.Values(funs)), nil
}

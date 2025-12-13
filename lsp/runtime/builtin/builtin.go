package builtin

import (
	_ "embed"
	"slices"

	"github.com/hephbuild/heph/lsp/runtime/query"
	"github.com/hephbuild/heph/lsp/runtime/symbol"
	tree_sitter "github.com/tree-sitter/go-tree-sitter"
)

//go:embed target.py
var target []byte

//go:embed helpers.py
var helpers []byte

//go:embed pybt.py
var pybt []byte

const builtinSource = "hbuiltins"

// ParseBuiltins Parses builtin files and extracts their symbols.
func ParseBuiltins(parser *tree_sitter.Parser) ([]*symbol.Symbol, error) {
	targetTree := parser.Parse(target, nil)
	defer targetTree.Close()

	helpersTree := parser.Parse(helpers, nil)
	defer helpersTree.Close()

	pybtTree := parser.Parse(pybt, nil)
	defer pybtTree.Close()

	targetSymbols, err := query.QuerySymbols(targetTree, target, builtinSource)
	if err != nil {
		return nil, err
	}

	helpersSymbols, err := query.QuerySymbols(helpersTree, helpers, builtinSource)
	if err != nil {
		return nil, err
	}

	pybtSymbols, err := query.QuerySymbols(pybtTree, pybt, builtinSource)
	if err != nil {
		return nil, err
	}

	return slices.Concat(targetSymbols, helpersSymbols, pybtSymbols), nil
}

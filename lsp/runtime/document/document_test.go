package document_test

import (
	_ "embed"
	"testing"

	"github.com/hephbuild/heph/lsp/runtime/document"
	"github.com/hephbuild/heph/lsp/runtime/symbol"
	"github.com/stretchr/testify/suite"

	tree_sitter "github.com/tree-sitter/go-tree-sitter"
	tree_sitter_python "github.com/tree-sitter/tree-sitter-python/bindings/go"
)

//go:embed testdata/test_document.py
var pythonTestFile []byte

type DocumentTestSuite struct {
	suite.Suite
}

func (s *DocumentTestSuite) TestNewDocument() {
	parser := tree_sitter.NewParser()
	err := parser.SetLanguage(tree_sitter.NewLanguage(tree_sitter_python.Language()))
	s.Require().NoError(err)

	tree := parser.Parse(pythonTestFile, nil)
	s.Require().NotNil(tree)

	doc, err := document.NewDocument("test.py", tree, pythonTestFile)
	s.Require().NoError(err)
	s.Require().NotNil(doc)

	s.Equal("test.py", doc.FullPath)
	s.NotNil(doc.Tree)
	s.NotNil(doc.Text)
	s.NotNil(doc.Symbols)
	s.NotEmpty(doc.Symbols)
}

func (s *DocumentTestSuite) TestSwapTree() {
	parser := tree_sitter.NewParser()
	err := parser.SetLanguage(tree_sitter.NewLanguage(tree_sitter_python.Language()))
	s.Require().NoError(err)

	tree := parser.Parse(pythonTestFile, nil)
	s.Require().NotNil(tree)

	doc, err := document.NewDocument("test.py", tree, pythonTestFile)
	s.Require().NoError(err)
	s.Require().NotNil(doc)

	initialSymbolCount := len(doc.Symbols)
	s.True(initialSymbolCount > 0, "Should have some symbols")

	newContent := []byte(`def new_function():
    return "new"

new_variable = 42`)

	newTree := parser.Parse(newContent, nil)
	s.Require().NotNil(newTree)

	oldTree, err := doc.SwapTree(newTree, newContent)
	s.Require().NoError(err)
	s.Require().NotNil(oldTree)

	s.Equal(string(newContent), doc.TextString)
	s.NotEmpty(doc.Symbols)

	foundSymbols := make([]string, 0, len(doc.Symbols))
	for _, sym := range doc.Symbols {
		foundSymbols = append(foundSymbols, sym.Name)
	}

	s.Contains(foundSymbols, "new_variable", "Should contain new variable")
}

func (s *DocumentTestSuite) TestQuery() {
	parser := tree_sitter.NewParser()
	err := parser.SetLanguage(tree_sitter.NewLanguage(tree_sitter_python.Language()))
	s.Require().NoError(err)

	tree := parser.Parse(pythonTestFile, nil)
	s.Require().NotNil(tree)

	doc, err := document.NewDocument("test.py", tree, pythonTestFile)
	s.Require().NoError(err)
	s.Require().NotNil(doc)

	sym, found := doc.Query("another_function")
	s.True(found, "Should find another_function")
	s.Require().NotNil(sym)
	s.Equal("another_function", sym.Name)
	s.Equal(symbol.FunctionKind, sym.Kind)

	sym, found = doc.Query("string_literal")
	s.True(found, "Should find string_literal")
	s.Require().NotNil(sym)
	s.Equal("string_literal", sym.Name)
	s.Equal(symbol.VariableKind, sym.Kind)

	sym, found = doc.Query("nonexistent")
	s.False(found, "Should not find nonexistent symbol")
	s.Nil(sym)
}

func TestDocumentTestSuite(t *testing.T) {
	suite.Run(t, new(DocumentTestSuite))
}

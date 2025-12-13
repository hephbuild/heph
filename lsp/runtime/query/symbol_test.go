package query_test

import (
	_ "embed"
	"testing"

	"github.com/hephbuild/heph/lsp/runtime/query"
	"github.com/stretchr/testify/suite"

	tree_sitter "github.com/tree-sitter/go-tree-sitter"
	tree_sitter_python "github.com/tree-sitter/tree-sitter-python/bindings/go"
)

//go:embed testdata/test_python_symbols.py
var pythonTestFile []byte

func findByteOffset(source []byte, searchStr string) int {
	for i := 0; i < len(source)-len(searchStr); i++ {
		if string(source[i:i+len(searchStr)]) == searchStr {
			return i
		}
	}
	return -1
}

type SymbolTestSuite struct {
	suite.Suite
	parser *tree_sitter.Parser
	tree   *tree_sitter.Tree
	root   *tree_sitter.Node
	source []byte
}

func (s *SymbolTestSuite) SetupSuite() {
	s.parser = tree_sitter.NewParser()
	err := s.parser.SetLanguage(tree_sitter.NewLanguage(tree_sitter_python.Language()))
	s.Require().NoError(err)
}

func (s *SymbolTestSuite) TearDownSuite() {
	if s.parser != nil {
		s.parser.Close()
	}
}

func (s *SymbolTestSuite) SetupTest() {
	s.tree = s.parser.Parse(pythonTestFile, nil)
	s.root = s.tree.RootNode()
	s.source = pythonTestFile
}

func (s *SymbolTestSuite) TearDownTest() {
	if s.tree != nil {
		s.tree.Close()
		s.tree = nil
		s.root = nil
	}
}

func (s *SymbolTestSuite) TestFindClassName() {
	currText := `
		class MyHephClass:
			pass
	`

	parser := tree_sitter.NewParser()
	err := parser.SetLanguage(tree_sitter.NewLanguage(tree_sitter_python.Language()))
	s.Require().NoError(err)

	tree := parser.Parse([]byte(currText), nil)
	defer tree.Close()
	root := tree.RootNode()

	byteOffset := uint(9)
	actualSymbol := query.ExtractCurrentSymbol(root, []byte(currText), byteOffset)

	expectedSymbol := "MyHephClass"
	s.Equal(expectedSymbol, actualSymbol)
}

func (s *SymbolTestSuite) TestFindVariable() {
	currText := `myvar = 12`

	parser := tree_sitter.NewParser()
	err := parser.SetLanguage(tree_sitter.NewLanguage(tree_sitter_python.Language()))
	s.Require().NoError(err)

	tree := parser.Parse([]byte(currText), nil)
	defer tree.Close()
	root := tree.RootNode()

	byteOffset := uint(2)
	actualSymbol := query.ExtractCurrentSymbol(root, []byte(currText), byteOffset)

	expectedSymbol := "myvar"
	s.Equal(expectedSymbol, actualSymbol)
}

func (s *SymbolTestSuite) TestFindFunctionNameFromParameter() {
	currText := "def my_fun(arg1, arg2):\n\tpass"

	parser := tree_sitter.NewParser()
	err := parser.SetLanguage(tree_sitter.NewLanguage(tree_sitter_python.Language()))
	s.Require().NoError(err)

	tree := parser.Parse([]byte(currText), nil)
	defer tree.Close()
	root := tree.RootNode()

	// arg1 offset a'r'g1
	byteOffset := uint(13)
	actualSymbol := query.ExtractFunctionNameFromOffset(root, []byte(currText), byteOffset)

	expectedSymbol := "my_fun"
	s.Equal(expectedSymbol, actualSymbol)

	// arg1 offset a'r'g2
	byteOffset = uint(19)
	actualSymbol = query.ExtractFunctionNameFromOffset(root, []byte(currText), byteOffset)
	s.Equal(expectedSymbol, actualSymbol)
}

func (s *SymbolTestSuite) TestFindArgumentNameFromCall() {
	currText := "my_fun(arg1=12, arg2=13)"

	parser := tree_sitter.NewParser()
	err := parser.SetLanguage(tree_sitter.NewLanguage(tree_sitter_python.Language()))
	s.Require().NoError(err)

	tree := parser.Parse([]byte(currText), nil)
	defer tree.Close()
	root := tree.RootNode()

	// 'r'
	byteOffset := uint(8)
	actualSymbol := query.ExtractCurrentSymbol(root, []byte(currText), byteOffset)

	expectedSymbol := "arg1"
	s.Equal(expectedSymbol, actualSymbol)

	// 'a'
	byteOffset = uint(16)
	actualSymbol = query.ExtractCurrentSymbol(root, []byte(currText), byteOffset)

	expectedSymbol = "arg2"
	s.Equal(expectedSymbol, actualSymbol)
}

func (s *SymbolTestSuite) TestFindFunctionNameNoParameter() {
	currText := "def my_fun():\n\tpass"

	parser := tree_sitter.NewParser()
	err := parser.SetLanguage(tree_sitter.NewLanguage(tree_sitter_python.Language()))
	s.Require().NoError(err)

	tree := parser.Parse([]byte(currText), nil)
	defer tree.Close()
	root := tree.RootNode()

	// '('
	byteOffset := uint(11)
	actualSymbol := query.ExtractFunctionNameFromOffset(root, []byte(currText), byteOffset)

	expectedSymbol := "my_fun"
	s.Equal(expectedSymbol, actualSymbol)

	// ')'
	byteOffset = uint(12)
	actualSymbol = query.ExtractFunctionNameFromOffset(root, []byte(currText), byteOffset)
	s.Equal(expectedSymbol, actualSymbol)
}

func (s *SymbolTestSuite) TestFindFunctionNameFromCall() {
	currText := "my_fun()"

	parser := tree_sitter.NewParser()
	err := parser.SetLanguage(tree_sitter.NewLanguage(tree_sitter_python.Language()))
	s.Require().NoError(err)

	tree := parser.Parse([]byte(currText), nil)
	defer tree.Close()
	root := tree.RootNode()

	// '('
	byteOffset := uint(6)
	actualSymbol := query.ExtractFunctionNameFromOffset(root, []byte(currText), byteOffset)

	expectedSymbol := "my_fun"
	s.Equal(expectedSymbol, actualSymbol)

	// ')'
	byteOffset = uint(7)
	actualSymbol = query.ExtractFunctionNameFromOffset(root, []byte(currText), byteOffset)
	s.Equal(expectedSymbol, actualSymbol)
}

func (s *SymbolTestSuite) TestExtractCurrentSymbol_FunctionName() {
	offset := findByteOffset(s.source, "hello_world")
	s.NotEqual(-1, offset, "Should find 'hello_world' in source")

	byteOffset := uint(offset)
	actualSymbol := query.ExtractCurrentSymbol(s.root, s.source, byteOffset)

	expectedSymbol := "hello_world"
	s.Equal(expectedSymbol, actualSymbol)
}

func (s *SymbolTestSuite) TestExtractCurrentSymbol_ClassName() {
	offset := findByteOffset(s.source, "MyHephClass")
	s.NotEqual(-1, offset, "Should find 'MyHephClass' in source")

	byteOffset := uint(offset)
	actualSymbol := query.ExtractCurrentSymbol(s.root, s.source, byteOffset)

	expectedSymbol := "MyHephClass"
	s.Equal(expectedSymbol, actualSymbol)
}

func (s *SymbolTestSuite) TestExtractCurrentSymbol_MethodName() {
	offset := findByteOffset(s.source, "my_method")
	s.NotEqual(-1, offset, "Should find 'my_method' in source")

	byteOffset := uint(offset)
	actualSymbol := query.ExtractCurrentSymbol(s.root, s.source, byteOffset)

	expectedSymbol := "my_method"
	s.Equal(expectedSymbol, actualSymbol)
}

func (s *SymbolTestSuite) TestExtractCurrentSymbol_VariableName() {
	offset := findByteOffset(s.source, "my_variable")
	s.NotEqual(-1, offset, "Should find 'my_variable' in source")

	byteOffset := uint(offset)
	actualSymbol := query.ExtractCurrentSymbol(s.root, s.source, byteOffset)

	expectedSymbol := "my_variable"
	s.Equal(expectedSymbol, actualSymbol)
}

func (s *SymbolTestSuite) TestExtractCurrentSymbol_FunctionParameter() {
	offset := findByteOffset(s.source, "param1")
	s.NotEqual(-1, offset, "Should find 'param1' in source")

	byteOffset := uint(offset)
	actualSymbol := query.ExtractCurrentSymbol(s.root, s.source, byteOffset)

	expectedSymbol := "param1"
	s.Equal(expectedSymbol, actualSymbol)
}

func (s *SymbolTestSuite) TestExtractCurrentSymbol_AnotherFunction() {
	offset := findByteOffset(s.source, "another_function")
	s.NotEqual(-1, offset, "Should find 'another_function' in source")

	byteOffset := uint(offset)
	actualSymbol := query.ExtractCurrentSymbol(s.root, s.source, byteOffset)

	expectedSymbol := "another_function"
	s.Equal(expectedSymbol, actualSymbol)
}

func (s *SymbolTestSuite) TestExtractCurrentSymbol_StaticMethod() {
	offset := findByteOffset(s.source, "static_method")
	s.NotEqual(-1, offset, "Should find 'static_method' in source")

	byteOffset := uint(offset)
	actualSymbol := query.ExtractCurrentSymbol(s.root, s.source, byteOffset)

	expectedSymbol := "static_method"
	s.Equal(expectedSymbol, actualSymbol)
}

func (s *SymbolTestSuite) TestExtractStringLiteral() {
	searchStr := "//heph/s"
	offset := findByteOffset(s.source, searchStr)
	s.NotEqual(-1, offset, "Should find '//heph/string/literal' in source")

	byteOffset := uint(offset)
	actualSymbol := query.ExtractCurrentStringLiteral(s.root, s.source, byteOffset)

	s.Equal("//heph/string/literal", actualSymbol)
}

func (s *SymbolTestSuite) TestExtractStringLiteral_NoString() {
	searchStr := "hello_world():"
	offset := findByteOffset(s.source, searchStr)
	s.NotEqual(-1, offset, "Should find 'hello_world():' in source")

	whitespaceOffset := offset + len(searchStr)
	s.True(whitespaceOffset < len(s.source), "Whitespace offset should be within bounds")

	byteOffset := uint(whitespaceOffset)
	actualSymbol := query.ExtractCurrentStringLiteral(s.root, s.source, byteOffset)

	s.Equal("", actualSymbol, "Should return empty string when cursor is not on a string literal")
}

func (s *SymbolTestSuite) TestExtractStringLiteral_OnVariable() {
	offset := findByteOffset(s.source, "my_variable")
	s.NotEqual(-1, offset, "Should find 'my_variable' in source")

	byteOffset := uint(offset)
	actualSymbol := query.ExtractCurrentStringLiteral(s.root, s.source, byteOffset)

	s.Equal("", actualSymbol, "Should return empty string when cursor is on a variable name")
}

func TestSymbolTestSuite(t *testing.T) {
	suite.Run(t, new(SymbolTestSuite))
}

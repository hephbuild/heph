package query_test

import (
	_ "embed"
	"testing"

	"github.com/hephbuild/heph/lsp/runtime/query"
	"github.com/hephbuild/heph/lsp/runtime/symbol"
	"github.com/stretchr/testify/suite"

	tree_sitter "github.com/tree-sitter/go-tree-sitter"
	tree_sitter_python "github.com/tree-sitter/tree-sitter-python/bindings/go"
)

//go:embed testdata/call_test.py
var callTest []byte

var expectedFunctionCalls = []string{"load", "print", "my_other_call", "fun_no_args", "target", "fun_with_no_args", "fun_with_no_args"}

type CallQuerySuite struct {
	suite.Suite
}

func (suite *CallQuerySuite) newParser() *tree_sitter.Parser {
	parser := tree_sitter.NewParser()

	err := parser.SetLanguage(tree_sitter.NewLanguage(tree_sitter_python.Language()))
	suite.Require().NoError(err)

	return parser
}

func (suite *CallQuerySuite) TestFunctionCallQuery() {
	parser := suite.newParser()
	pythonTree := parser.Parse(callTest, nil)

	symbols, err := query.QueryCalls(pythonTree, callTest, "")
	suite.Require().NoError(err)

	// Extract function call names
	callNames := []string{}
	for _, s := range symbols {
		callNames = append(callNames, s.Name)
	}

	suite.Require().NotNil(symbols)
	suite.Require().NotEmpty(symbols)
	suite.Require().ElementsMatch(expectedFunctionCalls, callNames)
}

func (suite *CallQuerySuite) TestFunctionCallParameters() {
	parser := suite.newParser()
	pythonTree := parser.Parse(callTest, nil)

	symbols, err := query.QueryCalls(pythonTree, callTest, "")
	suite.Require().NoError(err)

	// Create a map for easier lookup
	symbolMap := make(map[string]*symbol.Symbol)
	for _, s := range symbols {
		symbolMap[s.Name] = s
	}

	// Test load function parameters
	loadSymbol, ok := symbolMap["load"]
	suite.Require().True(ok, "load function should be found")
	suite.Require().Len(loadSymbol.Parameters, 2, "load should have 2 parameter")
	suite.Require().Equal("0", loadSymbol.Parameters[0].Name, "first parameter should be named '0'")
	suite.Require().Equal("\"//folder/to/load\"", loadSymbol.Parameters[0].Value, "load parameter should have correct value")
	suite.Require().Equal("1", loadSymbol.Parameters[1].Name, "first parameter should be named '1'")
	suite.Require().Equal("\"my_func\"", loadSymbol.Parameters[1].Value, "load parameter should have correct value")

	// Test print function parameters
	printSymbol, ok := symbolMap["print"]
	suite.Require().True(ok, "print function should be found")
	suite.Require().Len(printSymbol.Parameters, 1, "print should have 1 parameter")
	suite.Require().Equal("0", printSymbol.Parameters[0].Name, "first parameter should be named '0'")
	suite.Require().Equal("\"My Load\"", printSymbol.Parameters[0].Value, "print parameter should have correct value")

	// Test my_other_call function parameters
	myOtherCallSymbol, ok := symbolMap["my_other_call"]
	suite.Require().True(ok, "my_other_call function should be found")
	suite.Require().Len(myOtherCallSymbol.Parameters, 3, "my_other_call should have 3 parameters")
	suite.Require().Equal("0", myOtherCallSymbol.Parameters[0].Name, "first parameter should be named '0'")
	suite.Require().Equal("arg1", myOtherCallSymbol.Parameters[0].Value, "first parameter should have correct value")
	suite.Require().Equal("1", myOtherCallSymbol.Parameters[1].Name, "second parameter should be named '1'")
	suite.Require().Equal("kwarg1=\"literal\"", myOtherCallSymbol.Parameters[1].Value, "second parameter should have correct value")
	suite.Require().Equal("2", myOtherCallSymbol.Parameters[2].Name, "third parameter should be named '2'")
	suite.Require().Equal("kwarg2=12", myOtherCallSymbol.Parameters[2].Value, "third parameter should have correct value")
}

func (suite *CallQuerySuite) TestExtractFunctions() {
	parser := suite.newParser()
	pythonTree := parser.Parse(callTest, nil)

	symbols, err := query.QuerySymbols(pythonTree, callTest, "")
	suite.Require().NoError(err)

	// Find the my_func symbol
	var myFunc *symbol.Symbol
	for _, s := range symbols {
		if s.Name == "my_func" && s.Kind == symbol.FunctionKind {
			myFunc = s
			break
		}
	}
	suite.Require().NotNil(myFunc, "my_func should be found")

	// Check parameters
	suite.Require().Len(myFunc.Parameters, 3, "my_func should have 3 parameters")

	// param1: str
	suite.Require().Equal("param1", myFunc.Parameters[0].Name)
	suite.Require().Equal("str", myFunc.Parameters[0].Type)

	// param2: Union[str, int]
	suite.Require().Equal("param2", myFunc.Parameters[1].Name)
	suite.Require().Equal("Union[str, int]", myFunc.Parameters[1].Type)

	// param3: List[str] with default []
	suite.Require().Equal("param3", myFunc.Parameters[2].Name)
	suite.Require().Equal("List[str]", myFunc.Parameters[2].Type)
	suite.Require().Equal("[]", myFunc.Parameters[2].Value)
}

func TestCallQuerySuite(t *testing.T) {
	suite.Run(t, &CallQuerySuite{})
}

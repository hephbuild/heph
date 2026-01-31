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

//go:embed testdata/test.py
var pythonTest []byte

var (
	functionNames = []string{"my_custom_function", "my_other_function", "my_argless_function", "my_documented_function"}
	testVariables = []string{"my_custom_variable", "my_new_var", "my_custom_result"}
)

type QuerySuite struct {
	suite.Suite
}

func (suite *QuerySuite) newParser() *tree_sitter.Parser {
	parser := tree_sitter.NewParser()

	err := parser.SetLanguage(tree_sitter.NewLanguage(tree_sitter_python.Language()))
	suite.Require().NoError(err)

	return parser
}

func (suite *QuerySuite) TestFunctionQuery() {
	parser := suite.newParser()
	pythonTree := parser.Parse(pythonTest, nil)

	symbols, err := query.ExtractFunctions(pythonTree, pythonTest, "")
	suite.Require().NoError(err)

	names := []string{}
	for _, s := range symbols {
		names = append(names, s.Name)
	}

	suite.Require().NotNil(symbols)
	suite.Require().NotEmpty(symbols)
	suite.Require().ElementsMatch(functionNames, names)
}

func (suite *QuerySuite) TestFunctionParametersQuery() {
	parser := suite.newParser()
	pythonTree := parser.Parse(pythonTest, nil)

	symbols, err := query.ExtractFunctions(pythonTree, pythonTest, "")
	suite.Require().NoError(err)

	suite.Require().NotNil(symbols)
	suite.Require().NotEmpty(symbols)

	// Find the two functions
	var customFunc, otherFunc *symbol.Symbol
	for _, s := range symbols {
		switch s.Name {
		case "my_custom_function":
			customFunc = s
		case "my_other_function":
			otherFunc = s
		}
	}

	suite.Require().NotNil(customFunc, "my_custom_function should be found")
	suite.Require().NotNil(otherFunc, "my_other_function should be found")

	// Test my_custom_function parameters
	suite.Require().Len(customFunc.Parameters, 2, "my_custom_function should have 2 parameters")
	customFuncParamNames := []string{}
	for _, param := range customFunc.Parameters {
		customFuncParamNames = append(customFuncParamNames, param.Name)
	}
	suite.Require().Contains(customFuncParamNames, "arg1", "my_custom_function should have arg1 parameter")
	suite.Require().Contains(customFuncParamNames, "arg2", "my_custom_function should have arg2 parameter")

	// Test my_other_function parameters
	suite.Require().Len(otherFunc.Parameters, 4, "my_other_function should have 3 parameters")
	otherFuncParamNames := []string{}
	for _, param := range otherFunc.Parameters {
		otherFuncParamNames = append(otherFuncParamNames, param.Name)
	}
	suite.Require().Contains(otherFuncParamNames, "arg1", "my_other_function should have arg1 parameter")
	suite.Require().Contains(otherFuncParamNames, "arg2", "my_other_function should have arg2 parameter")
	suite.Require().Contains(otherFuncParamNames, "arg3", "my_other_function should have arg3 parameter")
	suite.Require().Contains(otherFuncParamNames, "arg4", "my_other_function should have arg4 parameter")
}

func (suite *QuerySuite) TestVariablesQuery() {
	parser := suite.newParser()
	pythonTree := parser.Parse(pythonTest, nil)

	symbols, err := query.ExtractVariables(pythonTree, pythonTest, "")
	suite.Require().NoError(err)

	names := []string{}
	for _, s := range symbols {
		names = append(names, s.Name)
	}

	suite.Require().NotNil(symbols)
	suite.Require().NotEmpty(symbols)
	suite.Require().ElementsMatch(testVariables, names)
}

func (suite *QuerySuite) TestFunctionArgsDoc() {
	parser := suite.newParser()
	pythonTree := parser.Parse(pythonTest, nil)

	symbols, err := query.ExtractFunctions(pythonTree, pythonTest, "")
	suite.Require().NoError(err)

	// Find my_documented_function
	var documentedFunc *symbol.Symbol
	for _, s := range symbols {
		if s.Name == "my_documented_function" {
			documentedFunc = s
			break
		}
	}
	suite.Require().NotNil(documentedFunc, "my_documented_function should be found")

	suite.Require().Len(documentedFunc.Parameters, 2, "my_documented_function should have 2 parameters")

	suite.Require().Equal("param1", documentedFunc.Parameters[0].Name)
	suite.Require().Equal("str", documentedFunc.Parameters[0].Type)
	suite.Require().Equal("The first parameter.", documentedFunc.Parameters[0].DocString)

	suite.Require().Equal("param2", documentedFunc.Parameters[1].Name)
	suite.Require().Equal("int", documentedFunc.Parameters[1].Type)
	suite.Require().Equal("The second parameter. Defaults to 0.", documentedFunc.Parameters[1].DocString)
	suite.Require().Equal("0", documentedFunc.Parameters[1].Value)
}

func TestQuerySuite(t *testing.T) {
	suite.Run(t, &QuerySuite{})
}

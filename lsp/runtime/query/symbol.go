package query

import tree_sitter "github.com/tree-sitter/go-tree-sitter"

// ExtractCurrentSymbol from root that is an identifier
func ExtractCurrentSymbol(root *tree_sitter.Node, source []byte, byteOffSet uint) string {
	for root != nil && root.Kind() != "identifier" {
		root = root.FirstChildForByte(byteOffSet)
	}

	if root == nil {
		return ""
	}

	return root.Utf8Text(source)
}

// ExtractCurrentStringLiteral finds closes string content
func ExtractCurrentStringLiteral(root *tree_sitter.Node, source []byte, byteOffSet uint) string {
	for root != nil && root.Kind() != "string_content" {
		root = root.FirstChildForByte(byteOffSet)
	}

	if root == nil {
		return ""
	}

	return root.Utf8Text(source)
}

// ExtractFunctionNameFromOffset extracts closest current function name whether its a call or definition
func ExtractFunctionNameFromOffset(root *tree_sitter.Node, source []byte, byteOffSet uint) string {
	childLookup := ""
	for root != nil {
		switch root.Kind() {
		case "function_definition":
			childLookup = "name"
		case "call":
			childLookup = "function"
		}

		if childLookup != "" {
			break
		}

		root = root.FirstChildForByte(byteOffSet)
	}

	if root == nil {
		return ""
	}

	root = root.ChildByFieldName(childLookup)

	if root == nil {
		return ""
	}

	return root.Utf8Text(source)
}

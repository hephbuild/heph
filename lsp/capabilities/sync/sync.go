package sync

import (
	"errors"

	"github.com/hephbuild/heph/lsp/runtime"
	"github.com/tliron/glsp"
	tree_sitter "github.com/tree-sitter/go-tree-sitter"

	protocol "github.com/tliron/glsp/protocol_3_16"
)

// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_synchronization

var (
	ErrInvalidTree = errors.New("invalid tree")
	ErrInvalidDoc  = errors.New("invalid doc")
)

func TextDocumentDidOpenWrapper(manager *runtime.Manager) protocol.TextDocumentDidOpenFunc {

	return func(context *glsp.Context, params *protocol.DidOpenTextDocumentParams) error {
		parser := manager.Parser
		text := params.TextDocument.Text
		bts := []byte(text)

		if doc, ok := manager.GetDocument(params.TextDocument.URI); ok {
			newTree := parser.Parse(bts, nil)
			if newTree == nil {
				return ErrInvalidTree
			}

			_, err := manager.SwapTree(doc, newTree, bts)

			return errors.Join(ErrInvalidTree, err)
		}

		newTree := parser.Parse(bts, nil)
		if newTree == nil {
			return ErrInvalidTree
		}

		version := params.TextDocument.Version
		_, err := manager.NewDocument(params.TextDocument.URI, newTree, bts, version)
		if err != nil {
			newTree.Close()
			return ErrInvalidTree
		}


		return nil
	}
}

func TextDocumentDidChangeFuncWrapper(manager *runtime.Manager) protocol.TextDocumentDidChangeFunc {
	return func(context *glsp.Context, params *protocol.DidChangeTextDocumentParams) error {
		parser := manager.Parser

		doc, ok := manager.GetDocument(params.TextDocument.URI)
		if !ok {
			return ErrInvalidDoc
		}

		for _, change := range params.ContentChanges {
			if event, ok := change.(protocol.TextDocumentContentChangeEvent); ok {
				text := doc.TextString
				insertBytes := []byte(event.Text)

				startByteOffset, endByteOffset := event.Range.IndexesIn(text)
				endByte := uint(endByteOffset) + uint(len(insertBytes))

				editInput := tree_sitter.InputEdit{
					StartByte:  uint(startByteOffset),
					OldEndByte: uint(endByteOffset),
					NewEndByte: endByte,

					StartPosition: tree_sitter.Point{
						Row:    uint(event.Range.Start.Line),
						Column: uint(event.Range.Start.Character),
					},
					OldEndPosition: tree_sitter.Point{
						Row:    uint(event.Range.End.Line),
						Column: uint(event.Range.End.Character),
					},
					NewEndPosition: tree_sitter.Point{
						Row:    uint(event.Range.End.Line),
						Column: endByte * 2,
					},
				}

				doc.Tree.Edit(&editInput)
				newText := ParseNewBytes(doc.Text, insertBytes, startByteOffset, endByteOffset)
				newTree := parser.Parse(newText, doc.Tree)

				_, err := manager.SwapTree(doc, newTree, newText)
				if err != nil {
					return errors.Join(ErrInvalidTree, err)
				}
			}

			if event, ok := change.(protocol.TextDocumentContentChangeEventWhole); ok {
				bts := []byte(event.Text)
				newTree := parser.Parse(bts, nil)

				_, err := manager.SwapTree(doc, newTree, bts)
				if err != nil {
					newTree.Close()
					return errors.Join(ErrInvalidTree, err)
				}
			}
		}

		return nil
	}
}

// ParseNewBytes creates a new byte array to insert changes from client
// Later we can check if we can do it inplace
func ParseNewBytes(current, insert []byte, offsetStart, offsetEnd int) []byte {
	diff := offsetEnd - offsetStart
	newLen := len(current) + len(insert) - diff
	newSlice := make([]byte, newLen)

	// Corner case: empty insert "" means delete
	if len(insert) == 0 {
		insertIndex := 0
		for i := 0; i < len(current); i++ {
			if i >= offsetStart && i < offsetEnd {
				continue
			}

			newSlice[insertIndex] = current[i]
			insertIndex++
		}

		return newSlice
	}

	// In the same request we can have Replace or Insert
	// Replace: offsetEnd > i, meaning we will overwrite any char until that offset
	// Insert:  Always insert
	insertIndex := 0
	currentIndex := 0
	for i := 0; i < newLen; i++ {
		if i >= offsetStart {
			// Replace
			if i < offsetEnd {
				currentIndex++

			}

			// Insert
			if insertIndex < len(insert) {
				newSlice[i] = insert[insertIndex]
				insertIndex++

				continue
			}
		}

		// Copy
		newSlice[i] = current[currentIndex]
		currentIndex++
	}

	return newSlice
}

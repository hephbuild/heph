package sync_test

import (
	"testing"

	"github.com/hephbuild/heph/lsp/capabilities/sync"
	"github.com/stretchr/testify/require"
)

func TestByteInsert(t *testing.T) {
	// UTF-8
	currText := "def hello_world():"
	newText := "_my_precious"
	currBytes := []byte(currText)
	newBytes := []byte(newText)
	insertedArray := sync.ParseNewBytes(currBytes, newBytes, 15, 15)

	expectedText := "def hello_world_my_precious():"
	actualText := string(insertedArray)
	require.Equal(t, expectedText, actualText)
}

func TestByteInsertWithNewLine(t *testing.T) {
	// UTF-8
	currText := "def hello_world():"
	newText := "_my_precious\n"
	currBytes := []byte(currText)
	newBytes := []byte(newText)
	insertedArray := sync.ParseNewBytes(currBytes, newBytes, 15, 15)

	expectedText := "def hello_world_my_precious\n():"
	actualText := string(insertedArray)
	require.Equal(t, expectedText, actualText)
}

func TestByteReplace(t *testing.T) {
	// UTF-8
	currText := "def hello_world():"
	newText := "world_hello"
	currBytes := []byte(currText)
	newBytes := []byte(newText)
	insertedArray := sync.ParseNewBytes(currBytes, newBytes, 4, 4+len(newBytes))

	expectedText := "def world_hello():"
	actualText := string(insertedArray)
	require.Equal(t, expectedText, actualText)
}

func TestByteReplaceWithInsert(t *testing.T) {
	// UTF-8
	currText := "def hello_friends():"
	newText := "hi_world"
	currBytes := []byte(currText)
	newBytes := []byte(newText)
	insertedArray := sync.ParseNewBytes(currBytes, newBytes, 4, 9)

	expectedText := "def hi_world_friends():"
	actualText := string(insertedArray)
	require.Equal(t, expectedText, actualText)
}

func TestByteRemove(t *testing.T) {
	// UTF-8
	currText := "def hello_world():"
	newText := ""
	currBytes := []byte(currText)
	newBytes := []byte(newText)
	insertedArray := sync.ParseNewBytes(currBytes, newBytes, 4, 10)

	expectedText := "def world():"
	actualText := string(insertedArray)
	require.Equal(t, expectedText, actualText)
}

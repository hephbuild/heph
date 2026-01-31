package document

import (
	"strings"
	"sync"
	"unsafe"

	"github.com/hephbuild/heph/lsp/runtime/builtin"
	"github.com/hephbuild/heph/lsp/runtime/query"
	"github.com/hephbuild/heph/lsp/runtime/symbol"
	tree_sitter "github.com/tree-sitter/go-tree-sitter"
)

type Document struct {
	// FullPath
	FullPath string

	Symbols []*symbol.Symbol
	Calls   []*symbol.Symbol

	// Loads are the BUILD path,
	// if any file has it name changed or delete,
	// Loads are not updated until next file sync, so use DocLoads
	Loads      []*RawLoad
	DocLoads   map[string]*Load
	IsLoadedBy map[string]*Load

	Tree       *tree_sitter.Tree
	Text       []byte // UTF-16 encoded byte array https://microsoft.github.io/language-server-protocol/specifications/specification-3-16/#textDocuments
	TextString string // UTF-16 encoded string https://microsoft.github.io/language-server-protocol/specifications/specification-3-16/#textDocuments

	treeMutex sync.Mutex
	loadsMu   sync.RWMutex
}

type RawLoad struct {
	Path  string
	Loads []string
}

type Load struct {
	Doc   *Document
	Loads []string
}

func (l *Load) Hash() string {
	return l.Doc.FullPath + "|" + strings.Join(l.Loads, "|")
}

// lockTwoLoads locks both docs mutexes in a deterministic way. Ugly, but keep the caller from undestading how call the locks
func lockTwoLoads(mu1, mu2 *sync.RWMutex) {
	p1 := uintptr(unsafe.Pointer(mu1))
	p2 := uintptr(unsafe.Pointer(mu2))
	if p1 < p2 {
		mu1.Lock()
		mu2.Lock()
	} else {
		mu2.Lock()
		mu1.Lock()
	}
}

func unlockTwoLoads(mu1, mu2 *sync.RWMutex) {
	mu1.Unlock()
	mu2.Unlock()
}

func (d *Document) Close() {
	d.Tree.Close()
}

func NewDocument(name string, tree *tree_sitter.Tree, rawText []byte) (*Document, error) {
	doc := &Document{
		FullPath: name, Tree: tree, Text: rawText, TextString: string(rawText),
		DocLoads: make(map[string]*Load), IsLoadedBy: make(map[string]*Load),
	}

	syms, calls, err := extractSymbols(doc.Tree, doc.Text, doc.FullPath)
	doc.Symbols = syms
	doc.Calls = calls
	doc.extractLoads()

	return doc, err
}

// SwapTree atomic swaps current tree and return the closed old tree
func (d *Document) SwapTree(newT *tree_sitter.Tree, newText []byte) (*tree_sitter.Tree, error) {
	d.treeMutex.Lock()
	defer d.treeMutex.Unlock()

	oldTree := d.Tree

	syms, calls, err := extractSymbols(newT, newText, d.FullPath)
	if err != nil {
		return nil, err
	}

	d.Symbols = syms
	d.Calls = calls
	d.Tree = newT
	d.Text = newText
	d.TextString = string(newText)

	d.resetLoads()
	d.extractLoads()

	oldTree.Close()

	return oldTree, err
}

func extractSymbols(tree *tree_sitter.Tree, text []byte, source string) ([]*symbol.Symbol, []*symbol.Symbol, error) {
	symbols, err := query.QuerySymbols(tree, text, source)
	if err != nil {
		return nil, nil, err
	}

	calls, err := query.QueryCalls(tree, text, source)

	return symbols, calls, err
}

func (d *Document) extractLoads() {
	loads := []*RawLoad{}
	for _, call := range d.Calls {
		if call.Name == builtin.LoadName {
			if len(call.Parameters) < 2 {
				continue
			}

			rawValue := call.Parameters[0].Value
			path := strings.Trim(rawValue, "\"")
			loadsSlice := []string{}
			for i := 1; i < len(call.Parameters); i++ {
				rawFunction := call.Parameters[i].Value
				fun := strings.Trim(rawFunction, "\"")
				loadsSlice = append(loadsSlice, fun)
			}
			loads = append(loads, &RawLoad{Path: path, Loads: loadsSlice})
		}
	}

	d.Loads = loads
}

func (d *Document) ExtractCurrentStringLiteral(byteOffSet uint) string {
	return query.ExtractCurrentStringLiteral(d.Tree.RootNode(), d.Text, byteOffSet)
}

func (d *Document) ExtractCurrentSymbol(byteOffSet uint) (*symbol.Symbol, bool) {
	sName := d.ExtractCurrentSymbolName(byteOffSet)
	return d.Query(sName)
}

func (d *Document) ExtractCurrentSymbolName(byteOffSet uint) string {
	return query.ExtractCurrentSymbol(d.Tree.RootNode(), d.Text, byteOffSet)
}

func (d *Document) ExtractCurrentFunctionName(byteOffSet uint) string {
	return query.ExtractFunctionNameFromOffset(d.Tree.RootNode(), d.Text, byteOffSet)
}

func (d *Document) Query(symbolName string) (*symbol.Symbol, bool) {
	return symbol.FindSymbol(d.Symbols, symbolName)
}

func (d *Document) QueryMany(symbolName []string) (*symbol.Symbol, bool) {
	return symbol.FindManySymbol(d.Symbols, symbolName)
}

func (d *Document) QueryAll(symbolName []string) []*symbol.Symbol {
	return symbol.FindManySymbols(d.Symbols, symbolName)
}

func (d *Document) QueryCalls(symbolName string) []*symbol.Symbol {
	return symbol.FindCalls(d.Calls, symbolName)
}

func (d *Document) RangeDocLoads(fn func(*Load)) {
	d.loadsMu.RLock()
	defer d.loadsMu.RUnlock()
	for _, doc := range d.DocLoads {
		fn(doc)
	}
}

func (d *Document) RangeIsLoadedBy(fn func(*Load)) {
	d.loadsMu.RLock()
	defer d.loadsMu.RUnlock()
	for _, doc := range d.IsLoadedBy {
		fn(doc)
	}
}

func (d *Document) resetLoads() {
	// Copy to remove from LoadedBy
	d.loadsMu.RLock()
	docs := make([]*Load, 0, len(d.DocLoads))
	for _, load := range d.DocLoads {
		docs = append(docs, load)
	}
	d.loadsMu.RUnlock()

	for _, load := range docs {
		doc := load.Doc
		doc.RemoveLoadedByDoc(d)
	}

	d.loadsMu.Lock()
	d.DocLoads = make(map[string]*Load)
	d.loadsMu.Unlock()
}

func (d *Document) AddLoadedDoc(doc *Document, loads []string) {
	// Locks both documents
	lockTwoLoads(&d.loadsMu, &doc.loadsMu)

	dl := &Load{Doc: doc, Loads: loads}
	d.DocLoads[dl.Hash()] = dl

	dlby := &Load{Doc: d, Loads: loads}
	doc.IsLoadedBy[dlby.Hash()] = dlby

	unlockTwoLoads(&d.loadsMu, &doc.loadsMu)
}

func (d *Document) RemoveLoadedDoc(doc *Document) {
	lockTwoLoads(&d.loadsMu, &doc.loadsMu)

	// remove from d.DocLoads
	for key, load := range d.DocLoads {
		if load.Doc == doc {
			delete(d.DocLoads, key)
			break
		}
	}

	// remove from doc.IsLoadedBy
	for key, load := range doc.IsLoadedBy {
		if load.Doc == d {
			delete(doc.IsLoadedBy, key)
			break
		}
	}

	unlockTwoLoads(&d.loadsMu, &doc.loadsMu)
}

func (d *Document) RemoveLoadedByDoc(doc *Document) {
	doc.loadsMu.Lock()

	for key, loader := range doc.IsLoadedBy {
		if loader.Doc == d {
			delete(doc.IsLoadedBy, key)
			break
		}
	}

	doc.loadsMu.Unlock()
}

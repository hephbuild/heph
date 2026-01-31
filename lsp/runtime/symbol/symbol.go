package symbol

type SymbolKind int

// Kind types
const (
	ClassKind SymbolKind = iota
	FunctionKind
	VariableKind
)

// Calls types
const (
	FunctionCallKind = iota + 4
	TargetCallKind
)

type Position struct {
	RowStart    uint
	ColumnStart uint
	RowEnd      uint
	ColumnEnd   uint
}

type rawPosition struct {
	ByteStart uint
	ByteEnd   uint
}

type Parameter struct {
	Name         string
	Type         string
	Value string
	DocString    string
}

type Symbol struct {
	Name   string
	Source string

	FullyQualifiedName string

	Kind      SymbolKind
	Signature string

	Parameters []*Parameter

	// Value is the current literal value for a variable
	Value     string
	DocString string

	Position Position

	SignaturePosition rawPosition

	Symbols []*Symbol
}

func (s *Symbol) Is(kind SymbolKind) bool {
	return s.Kind == kind
}

package symbol

import "slices"

func FindSymbol(symbols []*Symbol, sName string) (*Symbol, bool) {
	for _, symbol := range symbols {
		if symbol.FullyQualifiedName == sName {
			return symbol, true
		}

		if childS, found := FindSymbol(symbol.Symbols, sName); found {
			return childS, true
		}
	}

	return nil, false
}

func FindSymbols(symbols []*Symbol, sName string) []*Symbol {
	var result []*Symbol
	for _, symbol := range symbols {
		if symbol.FullyQualifiedName == sName {
			result = append(result, symbol)
		}

		childResults := FindSymbols(symbol.Symbols, sName)
		result = append(result, childResults...)
	}

	return result
}

func FindManySymbol(symbols []*Symbol, sName []string) (*Symbol, bool) {
	for _, symbol := range symbols {
		if slices.Contains(sName, symbol.FullyQualifiedName) {
			return symbol, true
		}

		cSym, found := FindManySymbol(symbol.Symbols, sName)
		if found {
			return cSym, true
		}
	}

	return nil, false
}

func FindManySymbols(symbols []*Symbol, sName []string) []*Symbol {
	var result []*Symbol
	for _, symbol := range symbols {
		if slices.Contains(sName, symbol.FullyQualifiedName) {
			result = append(result, symbol)
		}

		childResults := FindManySymbols(symbol.Symbols, sName)
		result = append(result, childResults...)
	}

	return result
}

func FindCalls(symbols []*Symbol, sName string) []*Symbol {
	var result []*Symbol
	for _, symbol := range symbols {
		if (symbol.Kind == FunctionCallKind || symbol.Kind == TargetCallKind) && symbol.FullyQualifiedName == sName {
			result = append(result, symbol)
		}

		childResults := FindCalls(symbol.Symbols, sName)
		result = append(result, childResults...)
	}
	return result
}

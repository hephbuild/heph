package specs

import (
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
)

type tokenType int

const (
	tokenEOF tokenType = iota
	tokenAnd
	tokenOr
	tokenNot
	tokenLParen
	tokenRParen
	tokenComma
	tokenLabel
	tokenAddr
	tokenFunction
	tokenString
)

type token struct {
	typ   tokenType
	value string
}

func isLabelChar(c uint8) bool {
	return ads.Contains(labelChars, rune(c)) || rune(c) == '*'
}

func isAddrChar(c uint8) bool {
	return ads.Contains(packageChars, rune(c)) ||
		ads.Contains(targetNameChars, rune(c)) ||
		rune(c) == '/' ||
		rune(c) == ':' ||
		rune(c) == '*'
}

func lex(input string) []token {
	var tokens []token

	for len(input) > 0 {
		switch {
		case input[0] == ' ' || input[0] == '\n':
			// skip whitespace
			input = input[1:]
		case input[0] == '&' && len(input) > 1 && input[1] == '&':
			tokens = append(tokens, token{typ: tokenAnd, value: "&&"})
			input = input[2:]
		case input[0] == '|' && len(input) > 1 && input[1] == '|':
			tokens = append(tokens, token{typ: tokenOr, value: "||"})
			input = input[2:]
		case input[0] == '!':
			tokens = append(tokens, token{typ: tokenNot, value: "!"})
			input = input[1:]
		case input[0] == '(':
			tokens = append(tokens, token{typ: tokenLParen, value: "("})
			input = input[1:]
		case input[0] == ')':
			tokens = append(tokens, token{typ: tokenRParen, value: ")"})
			input = input[1:]
		case input[0] == ',':
			tokens = append(tokens, token{typ: tokenComma, value: ","})
			input = input[1:]
		case input[0] == '"':
			i := 1
			for ; i < len(input); i++ {
				if input[i] == '"' {
					i++
					break
				}
			}
			tokens = append(tokens, token{typ: tokenString, value: input[1 : i-1]})
			input = input[i:]
		case isLabelChar(input[0]):
			i := 1
			for ; i < len(input); i++ {
				if !(isLabelChar(input[i])) {
					break
				}
			}

			if i < len(input) && input[i] == '(' {
				tokens = append(tokens, token{typ: tokenFunction, value: input[:i]})
				i++
			} else {
				tokens = append(tokens, token{typ: tokenLabel, value: input[:i]})
			}

			input = input[i:]
		case isAddrChar(input[0]):
			i := 1
			for ; i < len(input); i++ {
				if !(isAddrChar(input[i])) {
					break
				}
			}
			tokens = append(tokens, token{typ: tokenAddr, value: input[:i]})
			input = input[i:]
		default:
			panic(fmt.Sprintf("Unexpected character %v", string(input[0])))
		}
	}

	tokens = append(tokens, token{typ: tokenEOF, value: "EOF"})
	return tokens
}

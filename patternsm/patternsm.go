package patternsm

import "strings"

type Token interface {
	CanSwallow(Token) bool
	AtLeastOne() bool
	Multiple() bool
}

type tokenLiteral struct {
	c byte
}

func (l tokenLiteral) AtLeastOne() bool {
	return true
}

func (l tokenLiteral) Multiple() bool {
	return false
}

func (l tokenLiteral) CanSwallow(t Token) bool {
	switch t := t.(type) {
	case tokenLiteral:
		return t.c == l.c
	}

	return false
}

type tokenSlash struct {
}

func (l tokenSlash) AtLeastOne() bool {
	return true
}

func (l tokenSlash) Multiple() bool {
	return false
}

func (tokenSlash) CanSwallow(t Token) bool {
	switch t.(type) {
	case tokenSlash:
		return true
	}

	return false
}

type tokenStar struct {
}

func (l tokenStar) AtLeastOne() bool {
	return true
}

func (l tokenStar) Multiple() bool {
	return true
}

func (tokenStar) CanSwallow(t Token) bool {
	switch t.(type) {
	case tokenStar:
		return true
	case tokenLiteral:
		return true
	}

	return false
}

type tokenStarStar struct {
}

func (l tokenStarStar) AtLeastOne() bool {
	return true
}

func (l tokenStarStar) Multiple() bool {
	return true
}

func (tokenStarStar) CanSwallow(Token) bool {
	return true
}

func current(s, ss string) (bool, int) {
	if strings.HasPrefix(s, ss) {
		return true, len(ss)
	}

	return false, 0
}

func is(s, ss string) (bool, int) {
	if s == ss {
		return true, len(ss)
	}

	return false, 0
}

func parse(s string) []Token {
	tokens := make([]Token, 0, len(s))

	for i := 0; i < len(s); {
		s := s[i:]
		c := s[0]
		if ok, l := current(s, "/**/"); ok {
			tokens = append(tokens, tokenStarStar{})
			i += l
		} else if ok, l := is(s, "/**"); ok {
			tokens = append(tokens, tokenStarStar{})
			i += l
		} else if ok, l := is(s, "**/"); ok {
			tokens = append(tokens, tokenStarStar{})
			i += l
		} else if ok, l := current(s, "**"); ok {
			tokens = append(tokens, tokenStarStar{})
			i += l
		} else if ok, l := current(s, "*"); ok {
			tokens = append(tokens, tokenStar{})
			i += l
		} else if c == '/' {
			tokens = append(tokens, tokenSlash{})
			i += 1
		} else {
			tokens = append(tokens, tokenLiteral{c: c})
			i += 1
		}
	}

	return tokens
}

func Includes(a, b string) bool {
	at := parse(a)
	bt := parse(b)

	return match(at, bt, 0, 0, false)
}

func Intersects(a, b string) bool {
	at := parse(a)
	bt := parse(b)

	return match(at, bt, 0, 0, true)
}

func match(a, b []Token, ai, bi int, reverse bool) bool {
	if ai > len(a)-1 {
		return bi > len(b)-1
	}

	if bi > len(b)-1 {
		return ai > len(a)-1
	}

	as := a[ai].CanSwallow(b[bi])
	bs := reverse && b[bi].CanSwallow(a[ai])

	if as || bs {
		if match(a, b, ai+1, bi+1, reverse) {
			return true
		}
		if as && a[ai].Multiple() && match(a, b, ai, bi+1, reverse) {
			return true
		}
		if bs && b[bi].Multiple() && match(a, b, ai+1, bi, reverse) {
			return true
		}
	}

	if !as && !a[ai].AtLeastOne() {
		if match(a, b, ai+1, bi, reverse) {
			return true
		}
	}

	if reverse {
		if !bs && !b[bi].AtLeastOne() {
			if match(a, b, ai, bi+1, reverse) {
				return true
			}
		}
	}

	return false
}

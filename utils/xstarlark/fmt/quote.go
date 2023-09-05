package fmt

import (
	"strconv"
	"strings"
	"unicode/utf8"
)

func quoteWith(s string, q string) string {
	const hex = "0123456789abcdef"
	var runeTmp [utf8.UTFMax]byte

	var qq string
	for _, b := range q {
		qq += `\` + string(b)
	}

	buf := make([]byte, 0, 3*len(s)/2)
	buf = append(buf, q...)
	for width := 0; len(s) > 0; s = s[width:] {
		r := rune(s[0])
		width = 1
		if r >= utf8.RuneSelf {
			r, width = utf8.DecodeRuneInString(s)
		}
		if width == 1 && r == utf8.RuneError {
			// String (!b) literals accept \xXX escapes only for ASCII,
			// but we must use them here to represent invalid bytes.
			// The result is not a legal literal.
			buf = append(buf, `\x`...)
			buf = append(buf, hex[s[0]>>4])
			buf = append(buf, hex[s[0]&0xF])
			continue
		}
		if r == '\\' { // always backslashed
			buf = append(buf, '\\')
			buf = append(buf, byte(r))
			continue
		}
		if strings.HasPrefix(s, q) { // always backslashed
			buf = append(buf, qq...)
			width = len(q)
			continue
		}
		if strconv.IsPrint(r) {
			n := utf8.EncodeRune(runeTmp[:], r)
			buf = append(buf, runeTmp[:n]...)
			continue
		}
		switch r {
		case '\a':
			buf = append(buf, `\a`...)
		case '\b':
			buf = append(buf, `\b`...)
		case '\f':
			buf = append(buf, `\f`...)
		case '\n':
			buf = append(buf, `\n`...)
		case '\r':
			buf = append(buf, `\r`...)
		case '\t':
			buf = append(buf, `\t`...)
		case '\v':
			buf = append(buf, `\v`...)
		default:
			switch {
			case r < ' ' || r == 0x7f:
				buf = append(buf, `\x`...)
				buf = append(buf, hex[byte(r)>>4])
				buf = append(buf, hex[byte(r)&0xF])
			case r > utf8.MaxRune:
				r = 0xFFFD
				fallthrough
			case r < 0x10000:
				buf = append(buf, `\u`...)
				for s := 12; s >= 0; s -= 4 {
					buf = append(buf, hex[r>>uint(s)&0xF])
				}
			default:
				buf = append(buf, `\U`...)
				for s := 28; s >= 0; s -= 4 {
					buf = append(buf, hex[r>>uint(s)&0xF])
				}
			}
		}
	}
	buf = append(buf, q...)
	return string(buf)
}

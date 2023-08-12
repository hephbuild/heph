package xvt10x

import (
	"github.com/charmbracelet/lipgloss"
	"github.com/hinshun/vt10x"
)

// From github.com/hinshun/vt10x@v0.0.0-20220301184237-5011da428d02/state.go
const (
	attrReverse = 1 << iota
	attrUnderline
	attrBold
	attrGfx
	attrItalic
	attrBlink
	attrWrap
)

func renderGlyph(renderer *lipgloss.Renderer, attr vt10x.Glyph) string {
	s := lipgloss.NewStyle().Renderer(renderer)

	if attr.FG != vt10x.DefaultFG {
		s = s.Foreground(lipgloss.ANSIColor(attr.FG))
	}

	if attr.BG != vt10x.DefaultBG {
		s = s.Background(lipgloss.ANSIColor(attr.BG))
	}

	if attr.Mode&attrReverse != 0 {
		s = s.Reverse(true)
	}

	if attr.Mode&attrUnderline != 0 {
		s = s.Underline(true)
	}

	if attr.Mode&attrBold != 0 {
		s = s.Bold(true)
	}

	if attr.Mode&attrItalic != 0 {
		s = s.Italic(true)
	}

	if attr.Mode&attrBlink != 0 {
		s = s.Blink(true)
	}

	return s.Render(string([]rune{attr.Char}))
}

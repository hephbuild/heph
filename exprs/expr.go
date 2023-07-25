package exprs

import (
	"errors"
	"fmt"
	"strings"
)

type ExprArg struct {
	Name  string
	Value string
}

type Expr struct {
	String    string
	Function  string
	PosArgs   []string
	NamedArgs []ExprArg
}

func (e Expr) PosArg(n int, def string) string {
	if len(e.PosArgs) <= n {
		return def
	}

	return e.PosArgs[n]
}

func (e Expr) MustPosArg(n int) (string, error) {
	if len(e.PosArgs) <= n {
		return "", fmt.Errorf("arg %v is required", n)
	}

	return e.PosArgs[n], nil
}

func (e Expr) NamedArg(name string) string {
	for _, arg := range e.NamedArgs {
		if arg.Name == name {
			return arg.Value
		}
	}

	return ""
}

type Func func(expr Expr) (string, error)

const EOF = rune(-1)

func Exec(s string, funcs map[string]Func) (string, error) {
	e := executor{
		s:     []rune(s),
		funcs: funcs,
	}

	return e.exec()
}

var ErrNotExpr = errors.New("not an expr")

func Parse(s string) (Expr, error) {
	if !strings.HasPrefix(s, "$(") {
		return Expr{}, ErrNotExpr
	}

	e := executor{
		s: []rune(s),
	}

	if !e.isExpr() {
		return Expr{}, unexpected(e.cur(), e.i)
	}

	expr, err := e.parseExpr(false)
	if err != nil {
		return Expr{}, err
	}

	if e.cur() != EOF {
		return Expr{}, unexpected(e.cur(), e.i)
	}

	return expr, nil
}

type executor struct {
	s     []rune
	i     int
	funcs map[string]Func
}

func (p *executor) cur() rune {
	if p.i >= len(p.s) {
		return EOF
	}

	return p.s[p.i]
}

func (p *executor) curbtw(a, b rune) bool {
	return p.cur() >= a && p.cur() <= b
}

func (p *executor) peek(n int) rune {
	return p.s[p.i+n]
}

func (p *executor) next() {
	p.i++
}

func (p *executor) exec() (string, error) {
	var out strings.Builder
	out.Grow(len(p.s))

	for {
		if p.cur() == EOF {
			break
		} else if p.cur() == '$' && p.peek(1) == '$' {
			p.next()
			p.writeCur(&out)
		} else if p.isExpr() {
			s, err := p.parseExecExpr()
			if err != nil {
				return "", err
			}

			out.WriteString(s)
		} else {
			p.writeCur(&out)
		}
	}

	return out.String(), nil
}

func (p *executor) parseExecExpr() (string, error) {
	start := p.i
	e, err := p.parseExpr(true)
	if err != nil {
		return "", err
	}

	s, err := p.execExpr(e)
	if err != nil {
		return "", fmt.Errorf("%w at %v", err, start)
	}

	return s, nil
}

func (p *executor) execExpr(e Expr) (string, error) {
	f, ok := p.funcs[e.Function]
	if !ok {
		return "", fmt.Errorf("unknown function %v", e.Function)
	}

	s, err := f(e)
	if err != nil {
		return "", err
	}

	return s, nil
}

func (p *executor) isExpr() bool {
	i := p.i

	if p.cur() == '$' && p.peek(1) == '(' {
		p.next()
		p.next()
		ei := p.i

		if p.funcs != nil {
			ident := p.parseIdent()
			if ident == "" {
				p.i = i // reset
				return false
			}

			if _, ok := p.funcs[ident]; !ok {
				p.i = i // reset
				return false
			}
		}

		p.i = ei
		return true
	}

	return false
}

func (p *executor) isString() (bool, func(rune) bool) {
	ok := p.cur() == '"' || p.cur() == '\''
	if ok {
		until := p.cur()
		p.next()
		return true, func(r rune) bool {
			stop := r == until
			if stop {
				p.next()
				return true
			}

			return false
		}
	}

	if p.isIdentChar() {
		return true, func(r rune) bool {
			return r == ' ' || r == ')'
		}
	}

	return false, nil
}

func (p *executor) writeCur(sb *strings.Builder) {
	sb.WriteRune(p.cur())
	p.next()
}

func (p *executor) parseString(stop func(rune) bool) (string, error) {
	var sb strings.Builder

	for {
		if p.cur() == EOF {
			return "", unexpected(p.cur(), p.i)
		}

		if stop(p.cur()) {
			break
		}

		if p.cur() == '\\' {
			p.next()
		}

		sb.WriteRune(p.cur())

		p.next()
	}

	return sb.String(), nil
}

func (p *executor) isIdentChar() bool {
	return p.curbtw('a', 'z') || p.curbtw('A', 'Z') || p.curbtw('0', '9') || p.cur() == '_' || p.cur() == '-'
}

func (p *executor) parseIdent() string {
	start := p.i
	for p.isIdentChar() {
		p.next()
	}
	end := p.i

	return string(p.s[start:end])
}

type unexpectedErr struct {
	r  rune
	at int
}

func (e unexpectedErr) Error() string {
	s := fmt.Sprintf("`%v`", string(e.r))
	if e.r == EOF {
		s = "EOF"
	}

	return fmt.Sprintf("unexpected %v at %v", s, e.at)
}

func unexpected(r rune, at int) error {
	return unexpectedErr{
		r:  r,
		at: at,
	}
}

func (p *executor) parseExpr(allowExpr bool) (Expr, error) {
	e := Expr{}

	for {
		if p.cur() == ')' {
			if e.Function == "" {
				return Expr{}, unexpected(p.cur(), p.i)
			}

			p.next()
			return e, nil
		} else if p.cur() == ' ' {
			p.next()
		} else if p.isExpr() {
			if !allowExpr {
				return Expr{}, fmt.Errorf("expr is not allowed at %v", p.i)
			}

			s, err := p.parseExecExpr()
			if err != nil {
				return Expr{}, err
			}

			e.PosArgs = append(e.PosArgs, s)
		} else {
			ident := p.parseIdent()

			if e.Function == "" {
				if ident == "" {
					return Expr{}, unexpected(p.cur(), p.i)
				}

				e.Function = ident
				continue
			}

			if ident == "" {
				if ok, until := p.isString(); ok {
					s, err := p.parseString(until)
					if err != nil {
						return Expr{}, err
					}

					ident = s
				} else {
					return Expr{}, unexpected(p.cur(), p.i)
				}
			}

			if p.cur() == '=' {
				p.next()
				if p.isExpr() {
					val, err := p.parseExecExpr()
					if err != nil {
						return Expr{}, err
					}

					e.NamedArgs = append(e.NamedArgs, ExprArg{
						Name:  ident,
						Value: val,
					})
				} else if ok, until := p.isString(); ok {
					val, err := p.parseString(until)
					if err != nil {
						return Expr{}, err
					}

					e.NamedArgs = append(e.NamedArgs, ExprArg{
						Name:  ident,
						Value: val,
					})
				}
			} else {
				if len(e.NamedArgs) > 0 {
					return Expr{}, fmt.Errorf("positional args must come before named args")
				}

				e.PosArgs = append(e.PosArgs, ident)
			}

		}
	}
}

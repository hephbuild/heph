package exprs

import (
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"strings"
	"testing"
)

func TestExec(t *testing.T) {
	tests := []struct {
		e   string
		o   string
		err string
	}{
		{e: `hello`, o: `hello`},
		{e: `hel"lo`, o: `hel"lo`},
		{e: `hel()lo`, o: `hel()lo`},
		{e: `hel)lo`, o: `hel)lo`},
		{e: `hel$$(lo`, o: `hel$(lo`},
		{e: `hello $(world)`, o: `hello world`},
		{e: `hello $( world)`, o: `hello $( world)`},
		{e: `$(hello "world")`, o: `hello world`},
		{e: `$(hello world)`, o: `hello world`},
		{e: `$(hello 3)`, o: `hello 3`},
		{e: `$(hello "wo\"rld")`, o: `hello wo"rld`},
		{e: `$(hello "world $(hey)")`, o: `hello world $(hey)`},
		{e: `$(hello $(hello "world"))`, o: `hello hello world`},
		{e: `$(hello "$(hey)")`, o: `hello $(hey)`},
		{e: `$(dump)`, o: `dump||`},
		{e: `$(dump hello world)`, o: `dump|hello world|`},
		{e: `$(dump "hello" world)`, o: `dump|hello world|`},
		{e: `$(dump "hello" world="1")`, o: `dump|hello|world=1`},
		{e: `$(dump "hello" world=1)`, o: `dump|hello|world=1`},
		{e: `$(dump "hello" 1=a)`, o: `dump|hello|1=a`},
		{e: `$(dump "hello" 1=1)`, o: `dump|hello|1=1`},
		{e: `$(dump 1=1)`, o: `dump||1=1`},
		{e: `$(dump $(hello "1") arg1=$(hello "2") arg2=$(hello "3"))`, o: `dump|hello 1|arg1=hello 2 arg2=hello 3`},
		{e: `$(notdef)`, o: "$(notdef)"},
		{e: `$(notdef)`, o: "$(notdef)"},
		{e: `$(notdef $())`, o: "$(notdef $())"},
		{e: `$(notdef $(notdef))`, o: "$(notdef $(notdef))"},
		{e: `$(`, o: "$("},
		{e: `$()`, o: "$()"},

		{e: `$(hello "test)`, err: "unexpected EOF at 14"},
		{e: `$(hello a="a" "me")`, err: "positional args must come before named args"},
	}

	funcs := map[string]Func{
		"world": func(expr Expr) (string, error) {
			return "world", nil
		},
		"hello": func(expr Expr) (string, error) {
			return "hello " + expr.PosArgs[0], nil
		},
		"dump": func(expr Expr) (string, error) {
			nargs := make([]string, 0)
			for _, arg := range expr.NamedArgs {
				nargs = append(nargs, arg.Name+"="+arg.Value)
			}
			return "dump|" + strings.Join(expr.PosArgs, " ") + "|" + strings.Join(nargs, " "), nil
		},
	}

	for _, test := range tests {
		t.Run(test.e, func(t *testing.T) {
			t.Log(test.e)
			out, err := Exec(test.e, funcs)
			if test.err != "" {
				assert.EqualError(t, err, test.err)
			} else {
				require.NoError(t, err)
				assert.Equal(t, test.o, out)
			}
		})
	}
}

func TestParse(t *testing.T) {
	tests := []struct {
		e   string
		o   Expr
		err string
	}{
		{e: `$(hello)`, o: Expr{Function: "hello"}},
		{e: `$(hello "a" b="1")`, o: Expr{Function: "hello", PosArgs: []string{"a"}, NamedArgs: []ExprArg{{Name: "b", Value: "1"}}}},

		{e: `$(hello) `, err: "unexpected ` ` at 8"},
	}

	for _, test := range tests {
		t.Run(test.e, func(t *testing.T) {
			expr, err := Parse(test.e)
			if test.err != "" {
				assert.EqualError(t, err, test.err)
			} else {
				require.NoError(t, err)
				assert.Equal(t, test.o, expr)
			}
		})
	}
}

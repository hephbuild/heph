package fmt

import (
	"errors"
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"go.starlark.net/syntax"
	"io"
	"strconv"
	"strings"
	"unicode"
)

type Config struct {
	IndentSize int
}

func Fmt(path string, src io.Reader, cfg Config) (string, error) {
	return fmtSource(path, src, cfg)
}

const CommentSkipFile = "heph:fmt skip-file"

var ErrSkip = errors.New("skip file")

func fmtSource(path string, src io.Reader, cfg Config) (string, error) {
	if cfg.IndentSize <= 0 {
		return "", fmt.Errorf("indent must be > 0")
	}

	file, err := syntax.Parse(path, src, syntax.RetainComments)
	if err != nil {
		return "", err
	}

	if len(file.Stmts) > 0 {
		if commentsContains(file.Stmts[0], CommentSkipFile) {
			return "", ErrSkip
		}

		// This should cover the case of the last comment of a List which get stored as
		// an After to the file or Before of the next Node
		hoistCommentBackIntoList(&file, file)

		if file.Comments() != nil {
			// This should cover the case of the last comment of the file get stored as
			// an After to the file instead of an After to the previous statement
			hoistCommentBackIntoLastStmt(&file.Comments().After, ads.Last(file.Stmts))
		}
	}

	f := formatter{
		cfg:       cfg,
		indentStr: strings.Repeat(" ", cfg.IndentSize),
	}

	var buf Builder
	err = f.formatStmts(&buf, file.Stmts)

	formatAfterComments(&buf, file, nil)

	return buf.String(), err
}

func commentsContains(n syntax.Node, s string) bool {
	c := n.Comments()
	if c == nil {
		return false
	}

	for _, com := range c.Before {
		if strings.Contains(com.Text, s) {
			return true
		}
	}

	return false
}

type formatter struct {
	cfg       Config
	indentStr string
}

func is[T any](v any) bool {
	_, ok := v.(T)
	return ok
}

func isTargetLike(node syntax.Node) bool {
	expr, ok := node.(*syntax.CallExpr)
	if !ok {
		return false
	}

	if len(expr.Args) == 0 {
		return true
	}

	for _, arg := range expr.Args {
		e, ok := arg.(*syntax.BinaryExpr)
		if !ok || e.Op != syntax.EQ {
			return false
		}
	}

	return true
}

func shouldSkipLine(topLevel bool, prev, cur syntax.Node) bool {
	if is[*syntax.LoadStmt](prev) && is[*syntax.LoadStmt](cur) {
		return false
	}

	if is[*syntax.BranchStmt](cur) || is[*syntax.ReturnStmt](cur) {
		return true
	}

	if (topLevel && isTargetLike(prev)) ||
		(topLevel && isTargetLike(cur)) ||
		is[*syntax.LoadStmt](prev) ||
		is[*syntax.DefStmt](prev) ||
		is[*syntax.IfStmt](prev) ||
		is[*syntax.ForStmt](prev) {
		return true
	}

	if e, ok := prev.(*syntax.AssignStmt); ok {
		if shouldSkipLine(topLevel, e.RHS, cur) {
			return true
		}
	}

	if e, ok := prev.(*syntax.ExprStmt); ok {
		if shouldSkipLine(topLevel, e.X, cur) {
			return true
		}
	}

	prevLine := nodeEndLine(prev)
	curLine := nodeStartLine(cur)

	if prevLine+1 != curLine {
		return true
	}

	return false
}

func (f *formatter) formatStmts(w Writer, stmts []syntax.Stmt) error {
	for i, stmt := range stmts {
		if i < len(stmts)-1 {
			hoistCommentBackIntoList(&stmt, stmts[i+1])
		}

		if i > 0 && shouldSkipLine(isTopLevel(w), stmts[i-1], stmt) {
			w.WriteString("\n")
		}

		err := f.formatStmt(w, stmt)
		if err != nil {
			return err
		}
	}

	return nil
}

// Node Allow for custom type, copy of syntax.Node
type Node interface {
	Span() (start, end syntax.Position)
	Comments() *syntax.Comments
	AllocComments()
}

func formatBeforeComments(w Writer, stmt Node) {
	comz := stmt.Comments()
	if comz != nil {
		for _, c := range comz.Before {
			w.WriteString(formatComment(c.Text))
			w.WriteString("\n")
		}

		// Figure out if the comment and the stmt are separated by a newline
		if len(comz.Before) > 0 {
			lastCom := comz.Before[len(comz.Before)-1]

			stmtStart, _ := stmt.Span()
			if lastCom.Start.Line+1 != stmtStart.Line {
				w.WriteString("\n")
			}
		}
	}
}

func formatAfterComments(w Writer, stmt Node, afterNode func()) {
	comz := stmt.Comments()

	if comz != nil {
		if len(comz.Suffix) > 0 {
			w.WriteString(" ")
			w.WriteString(formatComment(comz.Suffix[0].Text))
		}
	}

	if afterNode != nil {
		afterNode()
	}

	if comz != nil {
		for _, c := range comz.After {
			w.WriteString(formatComment(c.Text))
			w.WriteString("\n")
		}
	}
}

func formatComment(s string) string {
	s = strings.TrimPrefix(s, "#")
	if len(s) > 0 {
		if s[0] != ' ' {
			s = " " + s
		}
	}
	s = strings.TrimRightFunc(s, unicode.IsSpace)

	return "#" + s
}

func (f *formatter) formatStmt(w Writer, sstmt syntax.Stmt) error {
	var stmt Node = sstmt
	switch sstmt := sstmt.(type) {
	case *syntax.IfStmt:
		stmt = resugarIfStmt(sstmt)
	}

	formatBeforeComments(w, stmt)
	defer formatAfterComments(w, stmt, func() {
		if !strings.HasSuffix(w.String(), "\n") {
			w.WriteString("\n")
		}
	})

	switch stmt := stmt.(type) {
	case *syntax.LoadStmt:
		w.WriteString("load(")
		w.WriteString(syntax.Quote(stmt.Module.Value.(string), false))
		for i := 0; i < len(stmt.From); i++ {
			w.WriteString(", ")

			from := stmt.From[i]
			to := stmt.To[i]

			if from.Name == to.Name {
				w.WriteString(syntax.Quote(from.Name, false))
			} else {
				err := f.formatExpr(w, to)
				if err != nil {
					return err
				}
				w.WriteString("=")
				w.WriteString(syntax.Quote(from.Name, false))
			}
		}
		w.WriteString(")")
	case *syntax.ExprStmt:
		formatBeforeComments(w, stmt.X)

		err := f.formatExpr(w, stmt.X)
		if err != nil {
			return err
		}

		formatAfterComments(w, stmt.X, nil)

		return nil
	case *syntax.AssignStmt:
		err := f.formatBinary(w, stmt.LHS, stmt.RHS, stmt.Op.String())
		if err != nil {
			return err
		}
	case *syntax.DefStmt:
		var argLastComs []syntax.Comment
		if len(stmt.Body) > 0 {
			argLastComs = extractHoistComments(stmt.Lparen, stmt.Rparen, stmt.Body[0])
		}

		w.WriteString("def ")
		err := f.formatExpr(w, stmt.Name)
		if err != nil {
			return err
		}
		err = f.formatList(w, stmt.Params, argLastComs, false, "()")
		if err != nil {
			return err
		}
		w.WriteString(":\n")
		err = f.formatStmts(f.indent(w, 1), stmt.Body)
		if err != nil {
			return err
		}
	case *syntax.ReturnStmt:
		if stmt.Result == nil {
			w.WriteString("return")

			return nil
		}

		w.WriteString("return ")
		return f.formatExpr(w, stmt.Result)
	case *IfStmt:
		for i, condStmt := range stmt.Ifs {
			formatBeforeComments(w, condStmt)

			token := "if"
			if i > 0 {
				token = "elif"
			}

			w.WriteString(token)
			w.WriteString(" ")
			err := f.formatExpr(w, condStmt.Cond)
			if err != nil {
				return err
			}
			w.WriteString(":\n")
			err = f.formatStmts(f.indent(w, 1), condStmt.True)
			if err != nil {
				return err
			}

			formatAfterComments(w, condStmt, nil)
		}

		if len(stmt.False) > 0 {
			w.WriteString("else:\n")
			err := f.formatStmts(f.indent(w, 1), stmt.False)
			if err != nil {
				return err
			}
		}
	case *syntax.ForStmt:
		w.WriteString("for ")
		err := f.formatExpr(w, stmt.Vars)
		if err != nil {
			return err
		}
		w.WriteString(" in ")
		err = f.formatExpr(w, stmt.X)
		if err != nil {
			return err
		}
		w.WriteString(":\n")
		err = f.formatStmts(f.indent(w, 1), stmt.Body)
		if err != nil {
			return err
		}
	case *syntax.BranchStmt:
		w.WriteString(stmt.Token.String())
	default:
		return fmt.Errorf("unhandled Stmt: %T", stmt)
	}
	return nil
}

func (f *formatter) formatBinary(w Writer, l, r syntax.Expr, s string) error {
	err := f.formatExpr(w, l)
	if err != nil {
		return err
	}

	w.WriteString(" ")
	w.WriteString(s)
	w.WriteString(" ")

	err = f.formatExpr(w, r)
	if err != nil {
		return err
	}

	return nil
}

const ListMaxLineLength = 100
const ListMaxCombinedLength = 50
const ListMaxElemLength = 50

func (f *formatter) formatList(w Writer, args []syntax.Expr, coms []syntax.Comment, nl bool, lr string) error {
	l, r := string(lr[0]), string(lr[1])

	if len(args) == 0 && len(coms) == 0 {
		w.WriteString(l)
		w.WriteString(r)
		return nil
	}

	if !nl {
		nl = len(coms) > 0
	}

	if !nl {
		for _, arg := range args {
			if com := arg.Comments(); com != nil {
				if com.Before != nil {
					nl = true
				}
				if com.After != nil {
					nl = true
				}
			}

			if nl {
				break
			}
		}
	}

	if !nl {
		length := 0
		length += 2 // for open/close

		var buf Builder
		for _, arg := range args {
			buf.Reset()

			err := f.formatExpr(&buf, arg)
			if err != nil {
				return err
			}

			s := buf.String()

			length += len(s)
			length += 2 // for separator

			if strings.Contains(s, "\n") {
				nl = true
			} else if len(s) > ListMaxElemLength && len(args) > 1 {
				nl = true
			} else if length > ListMaxCombinedLength && len(args) > 1 {
				nl = true
			} else if w.LineLength()+length > ListMaxLineLength {
				// Current arg makes it go over, while the existing line is under, make it multiline
				if w.LineLength() <= ListMaxLineLength {
					nl = true
				}
			}

			if nl {
				break
			}
		}
	}

	ow := w

	ow.WriteString(l)

	if nl {
		w = f.indent(ow, 1)

		w.WriteString("\n")
	}

	for i, arg := range args {
		if nl {
			formatBeforeComments(w, arg)
		}

		err := f.formatExpr(w, arg)
		if err != nil {
			return err
		}

		if nl || i != len(args)-1 {
			w.WriteString(",")
			if nl {
				formatAfterComments(w, arg, nil)
				w.WriteString("\n")
			} else {
				w.WriteString(" ")
			}
		}
	}

	if nl {
		for _, com := range coms {
			w.WriteString(formatComment(com.Text))
			w.WriteString("\n")
		}
	}

	ow.WriteString(r)

	return nil
}

func (f *formatter) formatExpr(w Writer, expr syntax.Expr) error {
	var listComs []syntax.Comment

	switch cexpr := expr.(type) {
	case *ListComsExpr:
		expr = cexpr.Expr
		listComs = cexpr.LastComs
	}

	switch expr := expr.(type) {
	case *syntax.CallExpr:
		err := f.formatExpr(w, expr.Fn)
		if err != nil {
			return err
		}

		nl := false
		if isTopLevel(w) {
			// If all args are named arguments, its most likely a target, force newline
			nl = isTargetLike(expr)
		}

		err = f.formatList(w, expr.Args, listComs, nl, "()")
		if err != nil {
			return err
		}
	case *syntax.Ident:
		w.WriteString(expr.Name)
	case *syntax.BinaryExpr:
		hoistCommentBackIntoList(&expr.X, expr.Y)

		err := f.formatBinary(w, expr.X, expr.Y, expr.Op.String())
		if err != nil {
			return err
		}
	case *syntax.Literal:
		switch expr.Token {
		case syntax.STRING:
			w.WriteStringRaw(quoteLiteral(expr))
		case syntax.BYTES:
			w.WriteString(syntax.Quote(expr.Value.(string), true))
		case syntax.INT:
			w.WriteString(strconv.FormatInt(expr.Value.(int64), 10))
		case syntax.FLOAT:
			w.WriteString(strconv.FormatFloat(expr.Value.(float64), 'f', -1, 64))
		default:
			return fmt.Errorf("unhandled Literal token: %v", expr.Token)
		}
	case *syntax.ListExpr:
		err := f.formatList(w, expr.List, listComs, false, "[]")
		if err != nil {
			return err
		}
	case *syntax.IndexExpr:
		err := f.formatExpr(w, expr.X)
		if err != nil {
			return err
		}
		w.WriteString("[")
		err = f.formatExpr(w, expr.Y)
		if err != nil {
			return err
		}
		w.WriteString("]")
	case *syntax.DotExpr:
		err := f.formatExpr(w, expr.X)
		if err != nil {
			return err
		}
		w.WriteString(".")
		err = f.formatExpr(w, expr.Name)
		if err != nil {
			return err
		}
	case *syntax.DictExpr:
		err := f.formatList(w, expr.List, listComs, len(expr.List) > 1, "{}")
		if err != nil {
			return err
		}
	case *syntax.DictEntry:
		err := f.formatExpr(w, expr.Key)
		if err != nil {
			return err
		}
		w.WriteString(": ")
		err = f.formatExpr(w, expr.Value)
		if err != nil {
			return err
		}
	case *syntax.Comprehension:
		l, r := "[", "]"
		if expr.Curly {
			l, r = "{", "}"
		}

		w.WriteString(l)

		err := f.formatExpr(w, expr.Body)
		if err != nil {
			return err
		}

		for _, clause := range expr.Clauses {
			w.WriteString(" ")

			err := f.formatClause(w, clause)
			if err != nil {
				return err
			}
		}

		w.WriteString(r)
	case *syntax.CondExpr:
		err := f.formatExpr(w, expr.True)
		if err != nil {
			return err
		}
		w.WriteString(" if ")

		err = f.formatExpr(w, expr.Cond)
		if err != nil {
			return err
		}

		if expr.False != nil {
			w.WriteString(" else ")
			err = f.formatExpr(w, expr.False)
			if err != nil {
				return err
			}
		}
	case *syntax.UnaryExpr:
		w.WriteString(expr.Op.String())
		if expr.Op < syntax.PLUS || expr.Op > syntax.STARSTAR {
			w.WriteString(" ")
		}
		err := f.formatExpr(w, expr.X)
		if err != nil {
			return err
		}
	case *syntax.TupleExpr:
		err := f.formatList(w, expr.List, listComs, false, "()")
		if err != nil {
			return err
		}
	case *syntax.ParenExpr:
		if t, ok := expr.X.(*syntax.TupleExpr); ok {
			return f.formatExpr(w, t)
		}

		w.WriteString("(")
		err := f.formatExpr(w, expr.X)
		if err != nil {
			return err
		}
		w.WriteString(")")
	default:
		return fmt.Errorf("unhandled Expr: %T", expr)
	}
	return nil
}

func (f *formatter) formatClause(w Writer, clause syntax.Node) error {
	switch clause := clause.(type) {
	case *syntax.ForClause:
		w.WriteString("for ")
		err := f.formatExpr(w, clause.Vars)
		if err != nil {
			return err
		}
		w.WriteString(" in ")
		err = f.formatExpr(w, clause.X)
		if err != nil {
			return err
		}
	case *syntax.IfClause:
		w.WriteString("if ")
		err := f.formatExpr(w, clause.Cond)
		if err != nil {
			return err
		}
	default:
		return fmt.Errorf("unhandled Clause: %T", clause)
	}
	return nil
}

func (f *formatter) indent(w Writer, n int) Writer {
	return &indentWriter{w: w, n: n, i: f.indentStr}
}

// quoteLiteral will select the best quote to prevent escaping
func quoteLiteral(expr *syntax.Literal) string {
	{
		s := expr.Value.(string)
		for _, r := range s {
			if !unicode.IsPrint(r) {
				return expr.Raw
			}
		}
	}

	{
		s := expr.Raw
		for i := 0; i < len(s); i++ {
			s := s[i:]

			if strings.HasPrefix(s, "\\\n") {
				return expr.Raw
			}
		}
	}

	s := expr.Value.(string)

	if strings.Contains(s, `"`) {
		if strings.Contains(s, `'`) {
			if strings.Contains(s, `"""`) {
				return quoteWith(s, `'''`)
			} else {
				return quoteWith(s, `"""`)
			}
		} else {
			return quoteWith(s, `'`)
		}
	} else {
		return quoteWith(s, `"`)
	}
}

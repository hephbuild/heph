package fmt

import "go.starlark.net/syntax"

var _ Node = (*IfStmt)(nil)

type IfStmt struct {
	start, end syntax.Position
	coms       syntax.Comments
	Cond       syntax.Expr
	True       []syntax.Stmt
	Elifs      []Elif
	False      []syntax.Stmt
}

func (s *IfStmt) Span() (start, end syntax.Position) {
	return s.start, s.end
}

func (s *IfStmt) Comments() *syntax.Comments {
	return &s.coms
}

type Elif struct {
	Cond syntax.Expr
	True []syntax.Stmt
}

func resugarIfStmt(stmt *syntax.IfStmt) *IfStmt {
	start, end := stmt.Span()

	elifStmts, elseStmts := resugarIfStmtFalse(stmt.False)

	sstmt := &IfStmt{
		start: start,
		end:   end,
		Cond:  stmt.Cond,
		True:  stmt.True,
		Elifs: elifStmts,
		False: elseStmts,
	}

	if coms := stmt.Comments(); coms != nil {
		sstmt.Comments().Before = coms.Before

		// Reassign suffix to last stmt
		if len(coms.Suffix) > 0 {
			var target syntax.Stmt
			if len(sstmt.False) > 0 {
				target = sstmt.False[len(sstmt.False)-1]
			} else if len(sstmt.Elifs) > 0 {
				e := sstmt.Elifs[len(sstmt.Elifs)-1]
				target = e.True[len(e.True)-1]
			} else {
				target = sstmt.True[len(sstmt.True)-1]
			}

			if target != nil {
				target.AllocComments()
				target.Comments().Suffix = coms.Suffix
			}

			coms.Suffix = nil // We cannot render them anyways...
		}
	}

	return sstmt
}

func resugarIfStmtFalse(stmts []syntax.Stmt) ([]Elif, []syntax.Stmt) {
	if len(stmts) == 0 {
		return nil, nil
	}

	elifs := make([]Elif, 0)

	falseStmts := stmts
	for len(falseStmts) > 0 {
		if len(stmts) > 1 {
			break
		}

		ifStmt, ok := falseStmts[0].(*syntax.IfStmt)
		if !ok {
			break
		}

		elifs = append(elifs, Elif{
			Cond: ifStmt.Cond,
			True: ifStmt.True,
		})

		falseStmts = ifStmt.False
	}

	return elifs, falseStmts
}

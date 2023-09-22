package fmt

import (
	"github.com/hephbuild/heph/utils/ads"
	"go.starlark.net/syntax"
)

var _ Node = (*IfStmt)(nil)

type IfStmt struct {
	start, end syntax.Position
	coms       syntax.Comments
	Ifs        []*IfCond
	False      []syntax.Stmt
}

func (s *IfStmt) Span() (start, end syntax.Position) {
	return s.start, s.end
}

func (s *IfStmt) Comments() *syntax.Comments {
	return &s.coms
}

func (s *IfStmt) AllocComments() {}

type IfCond struct {
	comments syntax.Comments
	Cond     syntax.Expr
	True     []syntax.Stmt
}

func (e *IfCond) Span() (start, end syntax.Position) {
	start, _ = e.Cond.Span()
	_, end = ads.Last(e.True).Span()
	return start, end
}

func (e *IfCond) Comments() *syntax.Comments {
	return &e.comments
}

func (e *IfCond) AllocComments() {}

func resugarIfStmt(stmt *syntax.IfStmt) *IfStmt {
	sstmt := resugarIfStmtFalse(stmt)

	if coms := stmt.Comments(); coms != nil {
		sstmt.Comments().Before = append(coms.Before, sstmt.Comments().Before...)
		sstmt.Comments().After = append(coms.After, sstmt.Comments().After...)

		// Reassign suffix to last stmt
		if len(coms.Suffix) > 0 {
			var target syntax.Stmt
			if len(sstmt.False) > 0 {
				target = ads.Last(sstmt.False)
			} else if len(sstmt.Ifs) > 0 {
				e := ads.Last(sstmt.Ifs)
				target = ads.Last(e.True)
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

func resugarIfStmtFalse(stmt *syntax.IfStmt) *IfStmt {
	start, end := stmt.Span()

	ifs := make([]*IfCond, 0)
	ifs = append(ifs, &IfCond{
		Cond: stmt.Cond,
		True: stmt.True,
	})

	sstmt := &IfStmt{
		start: start,
		end:   end,
	}

	falseStmts := stmt.False
	for len(falseStmts) > 0 {
		if len(falseStmts) > 1 {
			break
		}

		ifStmt, ok := falseStmts[0].(*syntax.IfStmt)
		if !ok {
			break
		}

		eif := &IfCond{
			Cond: ifStmt.Cond,
			True: ifStmt.True,
		}
		if ifStmt.Comments() != nil {
			hoistCommentBackIntoLastStmt(&ifStmt.Comments().Before, ads.Last(ifs))

			eif.comments = syntax.Comments{
				Before: ifStmt.Comments().Before,
			}
		}

		ifs = append(ifs, eif)

		falseStmts = ifStmt.False
		sstmt.Comments().After = nil
		if ifStmt.Comments() != nil {
			sstmt.Comments().After = ifStmt.Comments().After
		}
	}

	sstmt.Ifs = ifs
	sstmt.False = falseStmts

	return sstmt
}

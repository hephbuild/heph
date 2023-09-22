package fmt

import (
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/xtypes"
	"go.starlark.net/syntax"
)

func nodeStartLine(n Node) int32 {
	start, _ := n.Span()
	if coms := n.Comments(); coms != nil {
		if len(coms.Before) > 0 {
			start = coms.Before[0].Start
		}
	}

	return start.Line
}

func nodeEndLine(n Node) int32 {
	_, end := n.Span()
	if coms := n.Comments(); coms != nil {
		if len(coms.After) > 0 {
			end = coms.After[len(coms.After)-1].Start
		}
	}

	return end.Line
}

func lastListExprsFromNode[T Node](nodep *T) []*syntax.Expr {
	var node any = *nodep
	switch node := (node).(type) {
	case *ListComsExpr:
		var nodee syntax.Expr = node
		return append(lastListExprsFromExprs([]syntax.Expr{node.Expr}), &nodee)
	case *syntax.File:
		if len(node.Stmts) == 0 {
			return nil
		}

		var stmt Node = ads.Last(node.Stmts)

		return lastListExprsFromNode(&stmt)
	case *syntax.ExprStmt:
		return lastListExprsFromExpr(&node.X)
	case *syntax.DefStmt:
		if len(node.Body) == 0 {
			return nil
		}

		var stmt Node = ads.Last(node.Body)

		return lastListExprsFromNode(&stmt)
	case *syntax.WhileStmt:
		if len(node.Body) == 0 {
			return nil
		}

		var stmt Node = ads.Last(node.Body)

		return lastListExprsFromNode(&stmt)
	case *syntax.IfStmt:
		if len(node.False) > 0 {
			var last Node = ads.Last(node.False)
			return lastListExprsFromNode(&last)
		} else if len(node.True) > 0 {
			var last Node = ads.Last(node.True)
			return lastListExprsFromNode(&last)
		}

		return nil
	case *syntax.AssignStmt:
		return lastListExprsFromExpr(&node.RHS)
	case *syntax.ReturnStmt:
		return lastListExprsFromExpr(&node.Result)
	case syntax.Expr:
		return lastListExprsFromExpr(xtypes.ForceCast[*syntax.Expr](nodep))
	}

	return nil
}

func lastListExprsFromExprs[T []syntax.Expr](exprs T) []*syntax.Expr {
	if len(exprs) > 0 {
		return lastListExprsFromExpr(ads.LastP(exprs))
	}
	return nil
}

func lastListExprsFromExpr(exprp *syntax.Expr) []*syntax.Expr {
	switch expr := (*exprp).(type) {
	case *syntax.CallExpr:
		return append(lastListExprsFromExprs(expr.Args), exprp)
	case *syntax.DictExpr:
		return append(lastListExprsFromExprs(expr.List), exprp)
	case *syntax.DictEntry:
		return lastListExprsFromExpr(&expr.Value)
	case *syntax.ListExpr:
		return append(lastListExprsFromExprs(expr.List), exprp)
	case *syntax.TupleExpr:
		return append(lastListExprsFromExprs(expr.List), exprp)
	case *syntax.BinaryExpr:
		return append(lastListExprsFromExpr(&expr.X), lastListExprsFromExpr(&expr.Y)...)
	}

	return nil
}

type ListComsExpr struct {
	syntax.Expr
	LastComs []syntax.Comment
}

// Due to the way the AST stores comments, they can end up in the wrong location,
// this function tries to hoist them back into the correct place
func hoistCommentBackIntoList[T Node](nodeWhereComCouldBe *T, nodeWhereComIs Node) {
	if nodeWhereComIs.Comments() == nil {
		return
	}

	comDestExprs := lastListExprsFromNode(nodeWhereComCouldBe)

	for _, expr := range comDestExprs {
		if expr == nil || *expr == nil {
			continue
		}

		start, end := (*expr).Span()

		coms := extractHoistComments(start, end, nodeWhereComIs)
		if len(coms) == 0 {
			continue
		}

		switch exprObj := (*expr).(type) {
		case *ListComsExpr:
			exprObj.LastComs = append(exprObj.LastComs, coms...)
		default:
			*expr = &ListComsExpr{
				Expr:     exprObj,
				LastComs: coms,
			}
		}
	}
}

func extractHoistComments(start, end syntax.Position, nodeWhereComIs Node) []syntax.Comment {
	coms := nodeWhereComIs.Comments()

	if coms == nil {
		return nil
	}

	comStoreWhereComIs := &coms.Before
	// For file, potential comments are stored in .After
	if _, ok := nodeWhereComIs.(*syntax.File); ok {
		comStoreWhereComIs = &coms.After
	}

	if len(*comStoreWhereComIs) == 0 {
		return nil
	}

	extracted := make([]syntax.Comment, 0)
	for _, com := range *comStoreWhereComIs {
		// Is supposed to be part of previous expr
		if com.Start.Line >= start.Line && com.Start.Line <= end.Line {
			extracted = append(extracted, com)
		}
	}

	if len(extracted) == 0 {
		return nil
	}

	*comStoreWhereComIs = ads.Filter(*comStoreWhereComIs, func(com syntax.Comment) bool {
		return !ads.Contains(extracted, com)
	})

	return extracted
}

func lastStmtsFromNode(node Node) []Node {
	switch node := node.(type) {
	case *IfCond:
		return append(lastStmtsFromNode(ads.Last(node.True)), node)
	case *syntax.DefStmt:
		if len(node.Body) == 0 {
			return nil
		}

		stmt := ads.Last(node.Body)

		return append(lastStmtsFromNode(stmt), stmt, node)
	case *syntax.WhileStmt:
		if len(node.Body) == 0 {
			return nil
		}

		stmt := ads.Last(node.Body)

		return append(lastStmtsFromNode(stmt), stmt, node)
	case *syntax.IfStmt:
		if len(node.False) > 0 {
			last := ads.Last(node.False)
			return append(lastStmtsFromNode(last), last, node)
		} else if len(node.True) > 0 {
			last := ads.Last(node.True)
			return append(lastStmtsFromNode(last), last, node)
		}

		return nil
	case syntax.Stmt:
		return []Node{node}
	}

	return nil
}

func hoistCommentBackIntoLastStmt(comments *[]syntax.Comment, last Node) {
	if len(*comments) == 0 {
		return
	}

	lasts := lastStmtsFromNode(last)

	if len(lasts) == 0 {
		return
	}

	extracted := make([]syntax.Comment, 0)
comz:
	for _, com := range *comments {
		for _, last := range lasts {
			start, _ := last.Span()

			if com.Start.Col >= start.Col {
				extracted = append(extracted, com)

				last.AllocComments()
				last.Comments().After = append(last.Comments().After, com)
				continue comz
			}
		}
	}

	if len(extracted) == 0 {
		return
	}

	*comments = ads.Filter(*comments, func(com syntax.Comment) bool {
		return !ads.Contains(extracted, com)
	})
}

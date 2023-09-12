package fmt

import (
	"github.com/hephbuild/heph/utils/ads"
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

func lastListExprFromNode(node Node) *syntax.Expr {
	switch node := node.(type) {
	case *ListComsExpr:
		var nodee syntax.Expr = *node
		return &nodee
	case *syntax.File:
		if len(node.Stmts) == 0 {
			return nil
		}

		stmt := node.Stmts[len(node.Stmts)-1]

		return lastListExprFromNode(stmt)
	case *syntax.ExprStmt:
		return lastListExprFromExpr(&node.X)
	case *syntax.DefStmt:
		if len(node.Body) == 0 {
			return nil
		}

		stmt := node.Body[len(node.Body)-1]

		return lastListExprFromNode(stmt)
	case *syntax.AssignStmt:
		return lastListExprFromExpr(&node.RHS)
	}

	return nil
}

func lastListExprFromExpr(exprp *syntax.Expr) *syntax.Expr {
	switch expr := (*exprp).(type) {
	case *syntax.CallExpr:
		return exprp
	case *syntax.DictExpr:
		return exprp
	case *syntax.ListExpr:
		return exprp
	case *syntax.TupleExpr:
		return exprp
	case *syntax.BinaryExpr:
		return lastListExprFromExpr(&expr.Y)
	}

	return nil
}

type ListComsExpr struct {
	syntax.Expr
	LastComs []syntax.Comment
}

// Due to the way the AST stores comments, they can end up in the wrong location,
// this function tries to hoist them back into the correct place
func hoistCommentBackIntoList(nodeWhereComCouldBe, nodeWhereComIs Node) {
	comDestExpr := lastListExprFromNode(nodeWhereComCouldBe)
	if comDestExpr == nil || *comDestExpr == nil {
		return
	}

	start, end := (*comDestExpr).Span()

	coms := extractHoistComments(start, end, nodeWhereComIs)
	if len(coms) == 0 {
		return
	}

	switch expr := (*comDestExpr).(type) {
	case *ListComsExpr:
		expr.LastComs = append(expr.LastComs, coms...)
	default:
		*comDestExpr = &ListComsExpr{
			Expr:     *comDestExpr,
			LastComs: coms,
		}
	}
}

func extractHoistComments(start, end syntax.Position, nodeWhereComIs Node) []syntax.Comment {
	extracted := make([]syntax.Comment, 0)

	if coms := nodeWhereComIs.Comments(); coms != nil {
		comStoreWhereComIs := &coms.Before
		// For file, potential comments are stored in .After
		if _, ok := nodeWhereComIs.(*syntax.File); ok {
			comStoreWhereComIs = &coms.After
		}

		if len(*comStoreWhereComIs) == 0 {
			return nil
		}

		for _, com := range *comStoreWhereComIs {
			// Is supposed to be part of previous expr
			if com.Start.Line >= start.Line && com.Start.Line <= end.Line {
				extracted = append(extracted, com)
			}
		}

		*comStoreWhereComIs = ads.Filter(*comStoreWhereComIs, func(com syntax.Comment) bool {
			return !ads.Contains(extracted, com)
		})
	}

	return extracted
}

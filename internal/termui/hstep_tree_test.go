package termui

import (
	"strings"
	"testing"

	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/lipgloss/tree"
	"github.com/stretchr/testify/assert"
)

func TestRenderStepTree(t *testing.T) {
	tests := []struct {
		tree     *tree.Tree
		expected string
	}{
		{
			tree:     tree.New().Root("hello"),
			expected: `hello`,
		},
		{
			tree: tree.Root("").Child(tree.Root("root1")),
			expected: `
▢  root1
				`,
		},
		{
			tree: tree.Root("").
				Child(tree.Root("root1")).
				Child(tree.Root("root2")),
			expected: `
╭─ root1
╰─ root2
				`,
		},
		{
			tree: tree.Root("").
				Child(
					tree.Root("root1").Child("child1"),
				).
				Child(tree.Root("root2")),
			expected: `
╭─ root1
│  ╰─ child1
╰─ root2
		`,
		},
		{
			tree: tree.Root("").
				Child(
					tree.Root("root1").
						Child("child1").
						Child("child2"),
				).
				Child(tree.Root("root2")),
			expected: `
╭─ root1
│  ├─ child1
│  ╰─ child2
╰─ root2
		`,
		},
		{
			tree: tree.Root("").
				Child(
					tree.Root("root1").Child("child1"),
				),
			expected: `
▢  root1
   ╰─ child1
`,
		},
	}
	for _, test := range tests {
		t.Run(test.tree.String(), func(t *testing.T) {
			actual := renderStepsTree(lipgloss.NewRenderer(nil), test.tree)

			assert.Equal(t, strings.TrimSpace(test.expected), strings.TrimSpace(actual))
			if t.Failed() {
				t.Log("\n" + actual) // to make it easy to copy
			}
		})
	}
}

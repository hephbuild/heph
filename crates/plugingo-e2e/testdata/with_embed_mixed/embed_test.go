package embedmixed

import "testing"

func TestEmbeds(t *testing.T) {
	if Script == "" || Index == "" {
		t.Fatal("embeds empty")
	}
}

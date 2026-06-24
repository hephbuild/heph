package embedmixed

import _ "embed"

// Plain go_src static embed (file on disk, no target).
//go:embed resources/infhostd_ifplugd_nsg101.sh
var Script string

// go_embed_src lane embed (generated bundle, kept out of golist).
//go:embed ui_dist/index.html
var Index string

func Get() string { return Script + Index }

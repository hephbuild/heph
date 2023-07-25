package main

import (
	. "e2e/lib"
	"fmt"
)

func Scenario(run func() error, print bool, expectedUniqueFqn int, expectedLogBytes int64) {
	srv, cloud := NewHephcloudServer()
	defer srv.Close()

	Must(ReplaceFile(".hephconfig.local", "<URL>", srv.URL))

	Must(CleanSetup())

	Must(CloudLogin())

	fmt.Println("Running...")
	_ = run()

	fmt.Printf("Got %v spans\n", len(cloud.Spans()))
	fmt.Printf("Got %v bytes of span logs\n", cloud.LogBytes())

	var count int
	for fqn, spans := range cloud.SpansPerFQN() {
		if print {
			fmt.Println(fqn)
		}
		for _, span := range spans {
			if span.Event != "RUN_EXEC" {
				continue
			}
			count++
			if print {
				fmt.Println("   ", span)
			}
		}
	}

	if expectedUniqueFqn >= 0 {
		Must(AssertEqual(count, expectedUniqueFqn))
	}

	if expectedLogBytes >= 0 {
		Must(AssertEqual(cloud.LogBytes(), expectedLogBytes))
	}
}

package main

import (
	"cloud.google.com/go/bigquery"
	"github.com/apache/arrow/go/v12/arrow"
)

func main() {
	_ = bigquery.Scope
	_ = arrow.Second
}

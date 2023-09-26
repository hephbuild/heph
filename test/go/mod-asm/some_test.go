package main

import (
	"cloud.google.com/go/bigquery"
	"github.com/apache/arrow/go/v12/arrow"
	"testing"
)

func TestSanity(t *testing.T) {
	t.Log("Main - TestSanity Hello world")
	// Simply require the package so that all deps get pulled too
	_ = bigquery.Scope
	_ = arrow.Second
}

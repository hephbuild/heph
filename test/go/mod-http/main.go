package main

import (
	"fmt"
	"net/http"
)

func hello(w http.ResponseWriter, req *http.Request) {
	fmt.Println("hello")
	fmt.Fprintf(w, "hello\n")
}

func main() {
	fmt.Println("Hello world")

	http.HandleFunc("/hello", hello)

	http.ListenAndServe(":8090", nil)
}

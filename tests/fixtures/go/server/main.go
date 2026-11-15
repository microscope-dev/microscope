



package main

import (
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"os"
)


func greet(writer http.ResponseWriter, request *http.Request) {
	name := request.URL.Query().Get("name")
	fmt.Fprintf(writer, "hello, %s", name) // SERVER: greet
}


func broken(writer http.ResponseWriter, request *http.Request) {
	var counts map[string]int
	counts[request.URL.Path]++ // SERVER: broken
	fmt.Fprintln(writer, counts)
}

func main() {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	mux := http.NewServeMux()
	mux.HandleFunc("/greet", greet)
	mux.HandleFunc("/broken", broken)

	server := &http.Server{Handler: mux, ErrorLog: log.New(os.Stderr, "", 0)}
	go server.Serve(listener)

	base := "http://" + listener.Addr().String()
	for _, path := range []string{"/greet?name=gopher", "/broken"} {
		response, err := http.Get(base + path)
		if err != nil {
			fmt.Printf("%s: no response\n", path)
			continue
		}
		body, _ := io.ReadAll(response.Body)
		response.Body.Close()
		fmt.Printf("%s: %d %s\n", path, response.StatusCode, body)
	}
	server.Close()
}

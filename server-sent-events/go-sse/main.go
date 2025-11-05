package main

import (
	"fmt"
	"log"
	"net/http"
	"time"

	"github.com/tmaxmax/go-sse"
)

func main() {
	mux := http.NewServeMux()
	sseHandler := &sse.Server{}

	sseHandler.OnSession = func(w http.ResponseWriter, r *http.Request) (topics []string, allowed bool) {
		log.Printf("Client Connected\n")

		// Set CORS headers to allow requests from localhost:3000
		w.Header().Set("Access-Control-Allow-Origin", "*")
		w.Header().Set("Access-Control-Allow-Methods", "GET, OPTIONS")
		w.Header().Set("Access-Control-Allow-Headers", "Content-Type")

		// Start a goroutine to send messages after the connection is established
		go func() {
			// Give the connection a moment to fully establish
			time.Sleep(10 * time.Millisecond)

			for i := 10; i < 510; i += 10 {
				for j := 10; j < 510; j += 10 {
					ev := &sse.Message{}
					message := fmt.Sprintf("%03d,%03d", j, i)
					ev.AppendData(message)
					var err = sseHandler.Publish(ev)
					if err != nil {
						log.Printf("Error publishing message: %v\n", err)
						return
					}
					time.Sleep(1 * time.Millisecond)
				}
			}
			log.Printf("Finished sending all messages\n")
		}()

		// Return empty topics to subscribe to default/all messages
		return []string{}, true
	}

	server := &http.Server{
		Addr:    ":7999",
		Handler: mux,
	}

	mux.Handle("/", sseHandler)

	//nolint:gosec // Use http.Server in your code instead, to be able to set timeouts.
	if err := server.ListenAndServe(); err != nil {
		log.Fatalln(err)
	}
}

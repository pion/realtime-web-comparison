package main

import (
	"fmt"
	"log"
	"net/http"
	"runtime"

	"github.com/gorilla/websocket"
)

func main() {
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		var upgrader = websocket.Upgrader{
			CheckOrigin: func(r *http.Request) bool { return true },
		}

		conn, err := upgrader.Upgrade(w, r, nil)
		if err != nil {
			log.Fatal(err)
		}

		log.Println("Client Connected")
		for i := 10; i < 510; i += 10 {
			for j := 10; j < 510; j += 10 {
				message := fmt.Sprintf("%d,%d", j, i)
				if err := conn.WriteMessage(websocket.TextMessage, []byte(message)); err != nil {
					log.Fatal(err)
				}
				runtime.Gosched()
			}
		}
		if err := conn.Close(); err != nil {
			log.Fatal(err)
		}
	})
	log.Println("Server is listening at :8000")
	log.Fatal(http.ListenAndServeTLS(":8000",
		"../../certs/localhost.pem", "../../certs/localhost-key.pem", nil))
}

package main

import (
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"time"

	"github.com/gorilla/websocket"
	"github.com/pion/webrtc/v4"
)

// create global peer connection just for testing
// this is so that it can outlive the HTTP handler scope
var peerConn *webrtc.PeerConnection = nil

func encode(obj interface{}) string {
	b, err := json.Marshal(obj)
	if err != nil {
		log.Fatal(err)
	}
	return string(b)
}

func decode(in string, obj interface{}) {
	err := json.Unmarshal([]byte(in), obj)
	if err != nil {
		log.Fatal(err)
	}
}

func main() {
	config := webrtc.Configuration{
		ICEServers: []webrtc.ICEServer{
			{
				URLs: []string{"stun:stun.l.google.com:19302"},
			},
		},
	}

	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		var upgrader = websocket.Upgrader{
			CheckOrigin: func(r *http.Request) bool { return true },
		}

		// Check if this is a WebSocket upgrade request
		// This is so that we can visit this server in a browser to accept the TLS certificate
		if r.Header.Get("Upgrade") != "websocket" {
			// Regular HTTP request - just return OK so Chrome can accept the certificate
			w.WriteHeader(http.StatusOK)
			w.Write([]byte("WebRTC Signaling Server - Use WebSocket connection for signaling"))
			return
		}

		wsConn, err := upgrader.Upgrade(w, r, nil)
		if err != nil {
			log.Printf("WebSocket upgrade error: %v", err)
			return
		}

		log.Println("Client Connected")

		peerConn, err = webrtc.NewPeerConnection(config)
		if err != nil {
			log.Fatal(err)
		}
		peerConn.OnConnectionStateChange(func(s webrtc.PeerConnectionState) {
			log.Printf("Connection State: %s\n", s.String())

			if s == webrtc.PeerConnectionStateFailed {
				log.Fatal("Peer Connection failed")
			}
		})
		peerConn.OnICECandidate(func(i *webrtc.ICECandidate) {
			log.Printf("New ICE candidate: %v\n", i)
		})
		peerConn.OnDataChannel(func(channel *webrtc.DataChannel) {
			channel.OnOpen(func() {
				log.Println("Channel Open")
				for i := 10; i < 510; i += 10 {
					for j := 10; j < 510; j += 10 {
						message := fmt.Sprintf("%03d,%03d", j, i)
						if err := channel.Send([]byte(message)); err != nil {
							log.Fatal(err)
						}
						time.Sleep(1 * time.Millisecond)
					}
				}
				for {
					if channel.BufferedAmount() == 0 {
						if err := peerConn.Close(); err != nil {
							log.Fatal(err)
						}
						break
					}
					time.Sleep(1 * time.Millisecond)
				}
			})
		})

		var message []byte = nil
		for {
			_, message, err = wsConn.ReadMessage()
			if err != nil {
				log.Fatal(err)
			}
			if message != nil {
				break
			}
			time.Sleep(10 * time.Millisecond)
		}
		offer := webrtc.SessionDescription{}
		decode(string(message[:]), &offer)
		if err := peerConn.SetRemoteDescription(offer); err != nil {
			log.Fatal(err)
		}

		log.Printf("Client offer: %s\n", offer)

		answer, err := peerConn.CreateAnswer(nil)
		if err != nil {
			log.Fatal(err)
		}
		if err := peerConn.SetLocalDescription(answer); err != nil {
			log.Fatal(err)
		}

		gatherComplete := webrtc.GatheringCompletePromise(peerConn)
		<-gatherComplete

		log.Printf("Server answer: %s\n", *peerConn.LocalDescription())

		if err := wsConn.WriteMessage(websocket.TextMessage, []byte(encode(*peerConn.LocalDescription()))); err != nil {
			log.Fatal(err)
		}

		if err := wsConn.Close(); err != nil {
			log.Fatal(err)
		}
	})

	log.Println("Signaling server is listening at :8002")
	log.Fatal(http.ListenAndServeTLS(":8002",
		"../certs/localhost.pem", "../certs/localhost-key.pem", nil))
}

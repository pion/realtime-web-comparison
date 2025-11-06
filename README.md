# realtime-web-comparison
Experimenting with Server Sent Events, WebSocket, WebRTC, and WebTransport by streaming 2500 coordinates from server to client to visualize.

# NOTE: This repository is currently in flux.
- The WebTransport server currently does not work
- We're doing our best to update all code to the latest versions of their dependencies.

**Additional notes:**

- UDP Receive buffer size was incremented as suggested in https://github.com/lucas-clemente/quic-go/wiki/UDP-Receive-Buffer-Size

- No limits were specified on packet size or how protocols buffer packets.

The pseudo code of what each test is doing looks somewhat like this:

```go
for i := 10; i < 510; i += 10 {
    for j := 10; j < 510; j += 10 {
        message := fmt.Sprintf("%d,%d", j, i)
        if err := conn.WriteMessage(websocket.TextMessage, []byte(message)); err != nil {
            log.Fatal(err)
        }
        time.Sleep(1 * time.Millisecond)
    }
}
```

## TODO
- [Add more options for each network connection type](https://github.com/pion/realtime-web-comparison/issues/6)
- Add more libraries to test.

## Dependencies
- Server Sent Events
    - [tmaxmax/go-sse](https://github.com/tmaxmax/go-sse)
    - [tokio-rs/axum](https://github.com/tokio-rs/axum)
- WebSockets
    - [gorilla/websocket](https://github.com/gorilla/websocket)
    - [snapview/tungstenite-rs](https://github.com/snapview/tungstenite-rs)
- WebRTC
    - [pion/webrtc](https://github.com/pion/webrtc)
    - [algesten/str0m](https://github.com/algesten/str0m)
- WebTransport
    - [adriancable/webtransport-go](https://github.com/adriancable/webtransport-go)
- Client is written in pure HTML/CSS/JS.
    - For CSS: [Bootstrap](https://getbootstrap.com/)
    - The chart visualization is done using [Chart.js](https://www.chartjs.org/).

## Local testing

1. Clone repo
    ```bash
    git clone https://github.com/pion/realtime-web-comparison.git
    cd realtime-web-comparison
    ```

2. Create locally trusted certs using [mkcert](https://github.com/FiloSottile/mkcert)
   ```bash
   mkdir certs && cd certs
   mkcert -install
   mkcert localhost
   ```
    If using Nix flakes (see [Nix Setup Flakes](#nix-setup) section above):
   ```bash
   nix run .#setup-certs
   ```

3. Run a server (use similar commands for `webtransport` and `webrtc`)
    ```bash
    ./run.sh websocket
    ```

4. Simulating packet loss (use `del` instead of `add` to remove rules)
    ```bash
    sudo tc qdisc add dev lo root netem loss 15%
    ```

5. Run client
    ```bash
    ./run.sh client
    chromium --origin-to-force-quic-on=localhost:8001 http://localhost:3000
    ```

## Nix Setup

This repository includes a Nix flake for reproducible development environments. The flake provides all necessary dependencies including Rust, Go, OpenSSL, and mkcert.

### Development Shell

Enter the development shell to get all tools in your PATH:

```bash
nix develop
```

This provides:
- Rust toolchain (rustc, cargo, rustfmt, clippy, rust-analyzer)
- Go toolchain
- OpenSSL with proper environment variables configured
- mkcert for certificate generation
- NSS tools (for Firefox support on Linux)

### Setting Up Certificates

Use the Nix task to automatically set up locally trusted certificates:

```bash
nix run .#setup-certs
```

The `setup-certs` task will:
- Create the `certs/` directory if it doesn't exist
- Install the local CA certificate in your system trust store
- Generate a certificate for `localhost`

### Available Tasks

- `nix run .#setup-certs` - Set up development certificates using mkcert

Note: you might need to run `nix develop -c mkcert -install` and restart your browsers.
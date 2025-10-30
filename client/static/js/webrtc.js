import {chart, initCanvas, visualizePacket} from "./common.js";

const webRTCBtn = document.getElementById("webrtc");
const reliable = false;

webRTCBtn.onclick = (_) => {
    initCanvas();

    let t0 = new Date();
    let messageCount = 0;
    let iceFinished = false;
    console.info("Connecting to signaling server at wss://localhost:8002")
    const wsClient = new WebSocket("wss://localhost:8002");
    const conn = new RTCPeerConnection({iceServers: [{urls: 'stun:stun.l.google.com:19302'}]});
    const dataChannel = reliable ?
        conn.createDataChannel('dataChannel', {ordered: true, maxRetransmits: 5}) :
        conn.createDataChannel('dataChannel', {ordered: true, maxRetransmits: 0});
    const decoder = new TextDecoder("utf-8");

    // Add connection state logging
    conn.onconnectionstatechange = () => {
        console.info(`WebRTC Connection State: ${conn.connectionState}`);
    };
    
    conn.oniceconnectionstatechange = () => {
        console.info(`ICE Connection State: ${conn.iceConnectionState}`);
    };
    
    conn.onicegatheringstatechange = () => {
        console.info(`ICE Gathering State: ${conn.iceGatheringState}`);
    };

    wsClient.onopen = () => {
        console.info('WebSocket connected to signaling server');
    };

    wsClient.onerror = (err) => {
        console.error('WebSocket error:', err);
    };

    wsClient.onclose = () => {
        console.info('WebSocket connection closed');
    };

    console.info("Creating WebRTC offer...");
    conn.createOffer().then(o => {
        conn.setLocalDescription(o);
        console.info(`Created offer and set local description: ${o}`);
    });

    conn.onicecandidate = async e => {
        console.info(`New ice candidate: ${e.candidate}`)
        if (e.candidate === null) {
            iceFinished = true;
            while (wsClient.readyState !== 1) await new Promise(r => setTimeout(r, 10));
            wsClient.send(btoa(JSON.stringify(conn.localDescription)));
        }
    }

    dataChannel.onopen = () => {
        console.info(`WebRTC DataChannel established in ${new Date() - t0} ms.`);
    };

    dataChannel.onmessage = (e) => {
        if (messageCount === 0) {
            webRTCBtn.disabled = true;
            t0 = new Date();
            chart.data.datasets[2].data.push({x: 0, y: 0});
        }
        messageCount += 1;
        visualizePacket(decoder.decode(e.data));
        if (new Date() - t0 - chart.data.datasets[2].data.at(-1).x > 200) {
            chart.data.datasets[2].data.push({x: new Date() - t0, y: messageCount});
            chart.update();
        }
        if(messageCount === 2500) {
            chart.data.datasets[2].data.push({x: new Date() - t0, y: messageCount});
            chart.update();
        }
    }

    dataChannel.onclose = () => {
        console.info(`${messageCount} message(s) were received within ${new Date() - t0} ms.`)
        console.info('Disconnected from WebRTC server.');
    };

    wsClient.onmessage = (e) => {
        try {
            console.info(`Received message from signaling server: ${e.data}`);
            conn.setRemoteDescription(new RTCSessionDescription(JSON.parse(atob(e.data)))).then();
        } catch (e) {
            console.error(e);
        }
    }
}

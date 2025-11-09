import {chart, initCanvas, visualizePacket} from "./common.js";

const sseBtn = document.getElementById("sse");
const serverUrl = "http://localhost:7999";
const statsNumbers = document.getElementById("stats-numbers");

sseBtn.onclick = (_) => {
    initCanvas()
    console.info(`Connecting to Server Sent Events server at ${serverUrl} ...`);

    let t0 = new Date();
    let messageCount = 0;
    const eventSource = new EventSource(serverUrl);

    eventSource.onopen = (_) => {
        statsNumbers.textContent += `SSE Connection established in ${new Date() - t0} ms.`;
        sseBtn.disabled = true
        t0 = new Date();
        chart.data.datasets[3].data.push({x: 0, y: 0});
    }

    eventSource.onmessage = (e) => {
        messageCount += 1;
        visualizePacket(e.data);
        if (new Date() - t0 - chart.data.datasets[3].data.at(-1).x > 200) {
            chart.data.datasets[3].data.push({x: new Date() - t0, y: messageCount});
            chart.update();
        }
        
        // Check if we've received all messages (2500 total: 50x50)
        if (messageCount >= 2500) {
            eventSource.close();
            const timeElapsed = new Date() - t0;
            console.info(`${messageCount} message(s) were received within ${timeElapsed} ms.`)
            console.info('All messages received. Disconnected from Server Sent Events server.');
            statsNumbers.textContent += `Received ${messageCount} messages in ${timeElapsed} ms via SSE.`;
            sseBtn.disabled = false;
        }
    }

    eventSource.onerror = (err) => {
        console.error('SSE connection error:', err);
        eventSource.close();
        sseBtn.disabled = false;
        
        if (messageCount > 0) {
            // Connection dropped mid-stream
            chart.data.datasets[3].data.push({x: new Date() - t0, y: messageCount});
            chart.update();
            console.info(`Connection interrupted. ${messageCount} message(s) were received within ${timeElapsed} ms.`)
        }
    }
}

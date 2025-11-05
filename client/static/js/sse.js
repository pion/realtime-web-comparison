import {chart, initCanvas, visualizePacket} from "./common.js";

const sseBtn = document.getElementById("sse");
const serverUrl = "http://localhost:7999";

sseBtn.onclick = (_) => {
    initCanvas()
    console.info(`Connecting to Server Sent Events server at ${serverUrl} ...`);

    let t0 = new Date();
    let messageCount = 0;
    const eventSource = new EventSource(serverUrl);

    eventSource.onopen = (_) => {
        console.info(`Connection established in ${new Date() - t0} ms.`);
        sseBtn.disabled = true
        t0 = new Date();
        chart.data.datasets[3].data.push({x: 0, y: 0});
    }

    eventSource.onmessage = (e) => {
        // If event source messages are null, assume connection has been closed
        if (!e) {
            eventSource.close();
            chart.data.datasets[3].data.push({x: new Date() - t0, y: messageCount});
            chart.update();
            console.info(`${messageCount} message(s) were received within ${new Date() - t0} ms.`)
            console.info('Disconnected from Server Sent Events server.');
            return;
        }
        else {
            messageCount += 1;
            visualizePacket(e.data);
            if (new Date() - t0 - chart.data.datasets[3].data.at(-1).x > 200) {
                chart.data.datasets[3].data.push({x: new Date() - t0, y: messageCount});
                chart.update();
            }
        }
    }

    eventSource.onerror = (_) => {
        console.error('Failed to connect to Server Sent Events server');
    }
}

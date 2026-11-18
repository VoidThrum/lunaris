import QtQuick
import QtTest
import "../../qml"

// main.qml itself, off screen, with the stand-ins of stubs/: what the stats
// overlay says of the encoder and the decoder ("Encode", "Decode"), from
// what the bridge reports (the host's encoder status, the decoder's stats).
Item {
    id: root
    width: 200
    height: 100

    TestCase {
        name: "StatsOverlay"
        when: windowShown

        property var app: null
        property var bridge: null

        function init() {
            I18n.lang = "en";
            var component = Qt.createComponent("../../qml/main.qml");
            compare(component.status, Component.Ready, component.errorString());
            app = component.createObject(null);
            verify(app !== null);
            bridge = findChild(app, "bridge");
            verify(bridge !== null);
        }

        function cleanup() {
            if (app) app.destroy();
            app = null;
            I18n.lang = "en";
        }

        // The overlay's row with this label (in the language shown), as
        // { label, value, shown }; read anew each time (the rows are a binding).
        function row(label) {
            var rows = findChild(app, "statsOverlay").rows;
            for (var i = 0; i < rows.length; i++) {
                if (rows[i][0] === label) return { label: rows[i][0], value: rows[i][1], shown: rows[i][3] };
            }
            return null;
        }

        function stats(ping, decodeMs, backend) {
            bridge.statsUpdated(ping, decodeMs, 60, 12000, "h264", "h264", backend, "DMABUF/OpenGL", backend.indexOf("Software") !== 0, "None", 0, 0, 0, 0, "P2P (Direct)");
        }

        function test_gpu_names_are_shortened() {
            compare(app.shortGpuName("NVIDIA GeForce RTX 2060"), "RTX 2060");
            compare(app.shortGpuName("NVIDIA GeForce RTX 2060 (vendor=0x10de, device=0x1f08)"), "RTX 2060");
            compare(app.shortGpuName("AMD Radeon RX 6600/6600 XT/6600M"), "RX 6600/6600 XT/6600M");
            compare(app.shortGpuName("Intel(R) UHD Graphics 630 (vendor=0x8086, device=0x3e92)"), "UHD Graphics 630");
            // Only the maker is known: it stays.
            compare(app.shortGpuName("AMD GPU (amdgpu, vendor=0x1002, device=0x73ff)"), "AMD GPU");
            compare(app.shortGpuName("Unknown GPU (vendor=0x1234, device=0x0001)"), "");
            compare(app.shortGpuName("Unknown (encoded on the remote computer)"), "");
            compare(app.shortGpuName("Unknown"), "");
            compare(app.shortGpuName(""), "");
        }

        function test_encode_row() {
            verify(row("Encode") !== null);
            verify(row("Encode").shown);
            // Before the host said anything.
            compare(row("Encode").value, "-");
            bridge.encoderStatusUpdated("h264_nvenc", "NVENC", "NVIDIA GeForce RTX 2060", "auto", "linux", "", "");
            compare(row("Encode").value, "h264_nvenc · RTX 2060");
            bridge.encoderStatusUpdated("hevc_vaapi", "VAAPI", "AMD Radeon RX 6600/6600 XT/6600M", "auto", "linux", "", "");
            compare(row("Encode").value, "hevc_vaapi · RX 6600/6600 XT/6600M");
            // A software encoder: not the GPU the host happens to have.
            bridge.encoderStatusUpdated("libx264", "Software", "NVIDIA GeForce RTX 2060", "auto", "linux", "", "");
            compare(row("Encode").value, "libx264 · CPU");
            // No GPU named: the encoder alone (the last GPU heard of is
            // kept by the window, as before).
            app.agentGpuInfo = "Unknown";
            bridge.encoderStatusUpdated("h264_amf", "AMF", "", "auto", "windows", "", "");
            compare(row("Encode").value, "h264_amf");
        }

        function test_a_source_is_encoded_on_the_remote_computer() {
            // What the agent says of a session that shows another computer
            // through a connector (agent connector/media.rs `run`).
            bridge.encoderStatusUpdated("remote", "connector", "Unknown (encoded on the remote computer)", "", "windows", "", "");
            compare(row("Encode").value, "Remote PC");
            I18n.lang = "vi";
            verify(row("Mã hoá") !== null);
            compare(row("Mã hoá").value, "Máy từ xa");
        }

        function test_decode_row() {
            verify(row("Decode") !== null);
            verify(row("Decode").shown);
            // Before the first picture: only the time.
            compare(row("Decode").value, "0.0 ms");
            stats(3, 5.44, "VAAPI");
            compare(row("Decode").value, "VAAPI · 5.4 ms");
            stats(3, 2.1, "CUDA");
            compare(row("Decode").value, "NVDEC · 2.1 ms");
            stats(3, 6, "Software (dav1d)");
            compare(row("Decode").value, "Software (dav1d) · 6.0 ms");
            stats(3, 1.25, "D3D11VA");
            compare(row("Decode").value, "D3D11VA · 1.3 ms");
        }

        // One "Ping" row: what the bridge gives is the whole way already.
        function test_one_ping_row() {
            stats(12.4, 5, "VAAPI");
            compare(row("Ping").value, "12 ms");
            // Under ten milliseconds: with a decimal.
            stats(0.44, 5, "VAAPI");
            compare(row("Ping").value, "0.4 ms");
            stats(9.6, 5, "VAAPI");
            compare(row("Ping").value, "9.6 ms");
            stats(9.97, 5, "VAAPI");
            compare(row("Ping").value, "10 ms");
            var rows = findChild(app, "statsOverlay").rows;
            var pings = 0;
            for (var i = 0; i < rows.length; i++) {
                if (rows[i][0].indexOf("Ping") !== -1) pings++;
            }
            compare(pings, 1);
        }

        // The rows after "Codec": Encode, then Decode.
        function test_order() {
            var rows = findChild(app, "statsOverlay").rows;
            var labels = [];
            for (var i = 0; i < 6; i++) labels.push(rows[i][0]);
            compare(labels.join(","), "Ping,FPS,Bitrate,Codec,Encode,Decode");
        }
    }
}

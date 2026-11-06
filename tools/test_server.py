#!/usr/bin/env python3
"""接收端：收 ESP32-C3 发来的红外触发通知。UDP 和 TCP 两种模式。

用法：
    python3 test_server.py udp [port]    # 默认：监听 UDP 0.0.0.0:8080
    python3 test_server.py tcp [port]    # 监听 TCP 0.0.0.0:8080，接 HTTP POST

模板里的 TRANSPORT 改成哪个，就用对应模式跑这个脚本：
SERVER_IP 改成跑这个脚本的电脑的局域网 IP，SERVER_PORT 和端口对上。
"""

import json
import socket
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# 去重窗口（秒）：大于 10 次连发的 5 秒跨度，留足余量
DEDUP_WINDOW_S = 120


# ================= UDP：连发 10 次，接收端去重 =================
#
# 设备一次触发会连发 10 次（间隔 0.5 秒），接收端的工作就是：
# 收到 1 次算成功，其余 9 次认出来扔掉，一次触发只处理一次。
#
# 去重规则（按重要性排序）：
#   1. 去重键 = (来源 IP, count)。count 是设备上每次触发 +1 的全局序号。
#   2. 120 秒内见过的 (IP, count) 是重复，直接丢弃。
#   3. count 比已处理的小，是迟到的旧包，也丢弃。但这条只在 120 秒内
#      有效：超过 120 秒没收到任何包，认为是设备重启了（count 会从 0
#      重新开始），这时小 count 按新事件处理，不再当旧包扔。
#   4. 设备重启后 count 会从 0 重新开始：旧 count 早已过 120 秒窗口，
#      加上规则 3 的超时重置，不会被误判为重复，自然按新事件处理。
#
# 另外它还会统计每个 count 收到了几次（x/10），能看出来链路质量：
# 局域网里一般是 10/10，偶尔 9/10；如果经常只有 3/10，
# 说明 Wi-Fi 信号或路由器有问题，而不是程序问题。


def run_udp(port: int) -> None:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("0.0.0.0", port))
    print(f"Listening UDP 0.0.0.0:{port} ... (Ctrl+C to stop)", flush=True)

    seen: dict[tuple[str, int], float] = {}  # (来源 IP, count) -> 首次收到时间
    received: dict[int, int] = {}  # count -> 收到的次数
    last_processed: dict[str, tuple[int, float]] = {}  # 来源 IP -> (已处理的最新 count, 处理时间)

    while True:
        data, addr = sock.recvfrom(2048)
        src = addr[0]
        now = time.time()

        # 清理过期的去重记录
        for key in [k for k, t in seen.items() if now - t > DEDUP_WINDOW_S]:
            del seen[key]

        try:
            msg = json.loads(data.decode("utf-8"))
            count = int(msg["count"])
        except Exception:
            print(f"bad packet from {src}: {data!r}", flush=True)
            continue

        received[count] = received.get(count, 0) + 1

        # 规则 2：120 秒内见过就是重复
        key = (src, count)
        if key in seen:
            print(f"  duplicate ignored: count={count} ({received[count]}/10) from {src}", flush=True)
            continue
        seen[key] = now

        # 规则 3：迟到的旧包。但只在窗口内有效：超过窗口没收到包，
        # 认为是设备重启（count 从 0 开始），小 count 按新事件处理。
        last_entry = last_processed.get(src)
        last = None
        if last_entry is not None:
            last_count, last_time = last_entry
            if now - last_time <= DEDUP_WINDOW_S:
                last = last_count
        if last is not None and count <= last:
            print(f"  stale ignored: count={count} <= last={last} from {src}", flush=True)
            continue

        # 新事件：先把上一个 count 的最终收到数汇报一下，再处理这一次
        if last is not None:
            print(f"  (count={last} final: {received.get(last, 0)}/10)", flush=True)
        # 如果这个 count 之前出现过（设备重启后序号重来），收到数从 1
        # 重新计，避免和上一轮的统计混在一起
        if received[count] > 1:
            received[count] = 1
        last_processed[src] = (count, now)
        print(
            f"MOTION count={count} from {src} "
            f"uptime_ms={msg.get('uptime_ms')} "
            f"(received {received[count]}/10 so far)",
            flush=True,
        )


# ================= TCP：一次触发只发一个 POST，不用去重 =================
#
# TCP 是可靠连接，设备一次触发只 POST 一次，收到就是收到，
# 不用像 UDP 那样连发 + 去重。每次收到直接处理。


class TcpHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""
        try:
            msg = json.loads(body.decode("utf-8"))
            print(
                f"MOTION count={msg.get('count')} from {self.client_address[0]} "
                f"path={self.path}",
                flush=True,
            )
        except Exception:
            print(f"bad POST from {self.client_address[0]}: {body!r}", flush=True)
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"{}")

    def log_message(self, *args):
        pass  # 不输出 http.server 默认的访问日志，只打印我们关心的


def run_tcp(port: int) -> None:
    server = ThreadingHTTPServer(("0.0.0.0", port), TcpHandler)
    print(f"Listening TCP 0.0.0.0:{port} ... (Ctrl+C to stop)", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else "udp"
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 8080
    if mode == "tcp":
        run_tcp(port)
    elif mode == "udp":
        run_udp(port)
    else:
        print("Usage: test_server.py [udp|tcp] [port]", file=sys.stderr)
        sys.exit(2)

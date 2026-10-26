#!/usr/bin/env python3
"""假的云服务商 API，用于验证「云端部署费用确认」链路。

它不是产品代码，只是把不可能在无云账号环境下验证的路径变成可验证的：
真实 curl → 真实 HTTP → 真实 JSON 解析 → 真实打分 → 真实费用计算。

启动：python3 tests/fake-cloud-api.py <port>
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18100

# 三档 GPU，价格与显存不同，用来验证「按需选型 + 费用计算」
SPECS = [
    {
        "id": "gpu-4090-24g",
        "gpu_model": "RTX 4090",
        "gpu_count": 1,
        "vram_gb": 24,
        "cpu_cores": 16,
        "memory_gb": 64,
        "disk_gb": 200,
        "price": 1.85,
        "region": "cn-beijing",
        "stock": 3,
    },
    {
        "id": "gpu-5090-32g",
        "gpu_model": "RTX 5090",
        "gpu_count": 1,
        "vram_gb": 32,
        "cpu_cores": 20,
        "memory_gb": 96,
        "disk_gb": 300,
        "price": 2.60,
        "region": "cn-shanghai",
        "stock": 2,
    },
    {
        "id": "gpu-a100-40g",
        "gpu_model": "A100",
        "gpu_count": 1,
        "vram_gb": 40,
        "cpu_cores": 24,
        "memory_gb": 128,
        "disk_gb": 500,
        "price": 8.50,
        "region": "cn-hangzhou",
        "stock": 1,
    },
]


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _json(self, code, obj):
        b = json.dumps(obj, ensure_ascii=False).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        # 规格列表：字段名按内置 autodl profile 的 FieldMap 约定
        if "specs" in self.path or self.path.startswith("/v1/gpu"):
            self._json(
                200,
                {
                    "code": "Success",
                    "data": [
                        {
                            "spec_id": s["id"],
                            "gpu_name": s["gpu_model"],
                            "gpu_num": s["gpu_count"],
                            "gpu_memory": s["vram_gb"],
                            "cpu_num": s["cpu_cores"],
                            "memory": s["memory_gb"],
                            "disk": s["disk_gb"],
                            "price": s["price"],
                            "region": s["region"],
                            "stock": s["stock"],
                            "image": "pytorch:2.4-cuda12.1",
                        }
                        for s in SPECS
                    ],
                },
            )
            return
        # 实例查询
        self._json(200, {"code": "Success", "data": {"instances": []}})

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(n) or b"{}")
        self._json(
            200,
            {
                "code": "Success",
                "data": {
                    "instance_id": "fake-inst-001",
                    "status": "running",
                    "ssh_host": "127.0.0.1",
                    "ssh_port": 22,
                    "ssh_user": "root",
                },
            },
        )

    def do_DELETE(self):
        self._json(200, {"code": "Success"})


if __name__ == "__main__":
    print(f"fake cloud api on http://127.0.0.1:{PORT}", flush=True)
    HTTPServer(("127.0.0.1", PORT), H).serve_forever()

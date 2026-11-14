#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Fake der Azure Retail Prices API für die Browser-Tests (AIGW-85): feste Meter für swedencentral.

Aufruf: fake-retail.py PORT  – beantwortet jedes GET mit einer Seite (ohne NextPageLink).
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def item(sku, price, unit="1M"):
    return {"currencyCode": "USD", "retailPrice": price, "armRegionName": "swedencentral",
            "effectiveStartDate": "2026-03-01T00:00:00Z", "productName": "Azure OpenAI GPT5",
            "skuName": sku, "serviceName": "Foundry Models", "meterName": f"{sku} 1M Tokens",
            "unitOfMeasure": unit, "type": "Consumption"}


PAGE = json.dumps({"Items": [item("5.4 mini Inp Gl", 0.75), item("5.4 mini cd Inp Gl", 0.075),
                             item("5.4 mini Opt Gl", 4.5), item("5.4 mini Inp Dz", 0.825)],
                   "NextPageLink": None, "Count": 4}).encode()


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(PAGE)))
        self.end_headers()
        self.wfile.write(PAGE)

    def log_message(self, *_):
        pass


ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()

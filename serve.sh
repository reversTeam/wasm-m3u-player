#!/usr/bin/env bash
set -euo pipefail

PORT="${1:-9091}"

echo "Serving www/ on http://localhost:${PORT}"
echo "Headers: COOP=same-origin, COEP=require-corp"
echo "Press Ctrl+C to stop."

# Python 3 with COOP/COEP headers for SharedArrayBuffer support
cd www/
python3 -c "
import http.server
import functools

class CORSHandler(http.server.SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header('Cross-Origin-Opener-Policy', 'same-origin')
        self.send_header('Cross-Origin-Embedder-Policy', 'require-corp')
        self.send_header('Cache-Control', 'no-cache')
        super().end_headers()

    def guess_type(self, path):
        if path.endswith('.wasm'):
            return 'application/wasm'
        return super().guess_type(path)

handler = CORSHandler
server = http.server.HTTPServer(('', ${PORT}), handler)
server.serve_forever()
"

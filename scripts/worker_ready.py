# SPDX-License-Identifier: Apache-2.0

import ipaddress
from pathlib import Path
import re
import sys


def worker_ready_url(output):
    output = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", output)
    for line in output.splitlines(keepends=True):
        if not line.endswith(("\n", "\r")):
            continue
        ready = re.fullmatch(r"\s*(?:\[wrangler:info\]\s*)?Ready on (\S+)\s*", line)
        if not ready:
            if re.match(r"\s*(?:\[wrangler:info\]\s*)?Ready on\b", line):
                raise ValueError("Malformed Wrangler ready URL")
            continue
        url = ready[1]
        address = re.fullmatch(r"http://(\[[0-9a-fA-F:]+\]|[A-Za-z0-9.]+):([0-9]{1,5})", url)
        if not address or not 0 < int(address[2]) <= 65535:
            raise ValueError("Malformed Wrangler ready URL")
        host = address[1].strip("[]")
        if host.lower() != "localhost":
            try:
                loopback = ipaddress.ip_address(host).is_loopback
            except ValueError:
                loopback = False
            if not loopback:
                raise ValueError("Wrangler ready URL must use a loopback host")
        return url
    return None


if __name__ == "__main__":
    try:
        url = worker_ready_url(Path(sys.argv[1]).read_text(errors="replace"))
    except ValueError as error:
        print("error: " + str(error), file=sys.stderr)
        sys.exit(2)
    if url is None:
        sys.exit(1)
    print(url)

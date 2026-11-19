#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import json
import re
import sys
from pathlib import Path

SECTION = re.compile(r"^---- (\S+) stdout ----$", re.M)
FAILURES = re.compile(r"^failures:$", re.M)
SAVED = re.compile(r"^proptest regression saved in (\S+): cc ([0-9a-f]+)$", re.M)
CASE_FAILS = re.compile(r"^\S+::\S+ case \S+ fails: ")
NEVER_RAN = re.compile(r"^\S+::\S+ never ran case \S+$", re.M)
HARNESS_REPORT = re.compile(r"^(\S+::\S+ case \S+ fails: |not recorded: |recorded but passed: )")
LOADED = re.compile(r"^proptest regression: cc ([0-9a-f]+)$", re.M)
COMMITTED = re.compile(r"\bcc-[0-9a-f]+\b")


def sections(log):
    headers = list(SECTION.finditer(log))
    for index, header in enumerate(headers):
        end = headers[index + 1].start() if index + 1 < len(headers) else len(log)
        closing = FAILURES.search(log, header.end(), end)
        yield header[1], log[header.end():closing.start() if closing else end]


def failure_message(output):
    lines = output.splitlines()
    start = next((index for index, line in enumerate(lines) if line.startswith("Test failed: ")), None)
    if start is None:
        start = next((index + 1 for index, line in enumerate(lines[:-1]) if " panicked at " in line), None)
    if start is None:
        return None
    message = [lines[start].removeprefix("Test failed: ")]
    for line in lines[start + 1:]:
        if not line.strip() or line.startswith(("minimal failing input:", "note: ", "thread '", "---- ")):
            break
        message.append(line)
    message = [line for line in message if not NEVER_RAN.match(line)]
    failing = next((index for index, line in enumerate(message) if CASE_FAILS.match(line)), None)
    if failing is None:
        return "\n".join(message)
    failure = [CASE_FAILS.sub("", message[failing])]
    reports = message[failing + 1:]
    while reports and not HARNESS_REPORT.match(reports[0]):
        failure.append(reports.pop(0))
    failure[-1] += "."
    return "\n".join(message[:failing] + failure
                     + [line for line in reports if not line.startswith("recorded but passed: ")])


def unsaved(log, properties):
    saved = {match[2] for match in SAVED.finditer(log)}
    found = []
    for test, body in sections(log):
        if SAVED.search(body):
            continue
        reports = [line for line in body.splitlines() if HARNESS_REPORT.match(line) or NEVER_RAN.match(line)]
        loaded = set(LOADED.findall(body))
        if reports:
            if not any(COMMITTED.search(line) for line in reports):
                continue
            zero = True
        elif loaded and test.rsplit("::", 1)[-1] in properties:
            zero = not loaded & saved
        else:
            continue
        found.append({"test": test, "message": failure_message(body), "zero": zero})
    return found


def main():
    logs, properties = Path(sys.argv[1]), Path(sys.argv[2])
    print(json.dumps([{"binary": log.stem.rsplit("-", 1)[0], **test}
                      for log in sorted(logs.glob("*.log"))
                      for test in unsaved(log.read_text(errors="replace"),
                                          set((properties / f"{log.stem}.txt").read_text().split()))]))


if __name__ == "__main__":
    main()

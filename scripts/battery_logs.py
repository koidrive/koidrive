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


def replayed(body):
    return any(HARNESS_REPORT.match(line) or NEVER_RAN.match(line) for line in body.splitlines())


def unsaved(log):
    tests = [(test, body, bool(SAVED.search(body))) for test, body in sections(log)]
    others_saved = any(saved for _, _, saved in tests)
    return [{"test": test, "message": failure_message(body), "zero": replayed(body) or not others_saved}
            for test, body, saved in tests if not saved]


def main():
    print(json.dumps([{"binary": log.stem.rsplit("-", 1)[0], **test}
                      for log in sorted(Path(sys.argv[1]).glob("*.log"))
                      for test in unsaved(log.read_text(errors="replace"))]))


if __name__ == "__main__":
    main()

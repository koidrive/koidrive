// SPDX-License-Identifier: AGPL-3.0-or-later

import { type CommitRecord, NodeKind, type NodeState, NodeStatus } from "../../src/gen/koidrive/v1/model_pb";

export function replay(log: CommitRecord[], seq: bigint): Map<string, NodeState> {
  const nodes = new Map<string, NodeState>();
  for (const commit of log) {
    if (commit.seq > seq) break;
    for (const { change } of commit.changes) {
      if (change.case === "node") nodes.set(change.value.nodeId, change.value);
      else if (change.case === "purged") nodes.delete(change.value.nodeId);
    }
  }
  return nodes;
}

function liveNodes(nodes: Map<string, NodeState>): string {
  return [...nodes.values()]
    .filter((node) => node.status === NodeStatus.LIVE && node.parentId !== "")
    .map((node) => [node.nodeId, node.parentId, node.name, node.kind, node.current?.versionId ?? ""].join(" ") + "\n")
    .sort()
    .join("");
}

export function rewindMismatch(before: CommitRecord[], after: CommitRecord[], tree: Map<string, NodeState>, restoreSeq: bigint): string | undefined {
  const head = (log: CommitRecord[]) => log.at(-1)?.seq ?? 0n;
  if (head(before) < restoreSeq) return "the rewind reports restore_seq " + restoreSeq + ", but the log before it ends at seq " + head(before);
  if (head(after) !== restoreSeq) return "the rewind reports restore_seq " + restoreSeq + ", but the log after it ends at seq " + head(after);
  const commits = (log: CommitRecord[]) => log.map((commit) => commit.seq + " " + commit.commitId).join(", ");
  const kept = commits(before.filter((commit) => commit.seq <= restoreSeq));
  if (commits(after) !== kept) return "the log after the rewind is [" + commits(after) + "], but the log before it up to seq " + restoreSeq + " is [" + kept + "]";
  const want = liveNodes(replay(before, restoreSeq));
  const got = liveNodes(tree);
  if (got !== want) return "the server's tree after the rewind is not the tree the log before it holds at seq " + restoreSeq + ".\nLog:\n" + want + "Server:\n" + got;
  return undefined;
}

function pathIn(tree: Map<string, NodeState>, node: NodeState): string {
  const parent = tree.get(node.parentId);
  return parent && parent.parentId !== "" ? pathIn(tree, parent) + "/" + node.name : node.name;
}

const byPath = (a: string, b: string) => (a < b ? -1 : a > b ? 1 : 0);

export function filePaths(tree: Map<string, NodeState>): [string, NodeState][] {
  return [...tree.values()]
    .filter((node) => node.status === NodeStatus.LIVE && node.kind === NodeKind.FILE)
    .map((node) => [pathIn(tree, node), node] as [string, NodeState])
    .sort(([a], [b]) => byPath(a, b));
}

export function lastCommitAtOrBefore(log: CommitRecord[], pointMs: bigint): bigint {
  const after = log.findIndex((commit) => commit.committedAtMs > pointMs);
  return (after < 0 ? log.at(-1) : log[after - 1])?.seq ?? 0n;
}

export function removedByRewind(log: CommitRecord[], restoreSeq: bigint): { items: string[]; files: string[] } {
  const head = replay(log, log.at(-1)?.seq ?? 0n);
  const restored = replay(log, restoreSeq);
  const removed = [...head.values()].filter((node) => node.status === NodeStatus.LIVE && node.parentId !== "" && !restored.has(node.nodeId));
  const ids = new Set(removed.map((node) => node.nodeId));
  const paths = (nodes: NodeState[]) => nodes.map((node) => pathIn(head, node)).sort(byPath);
  return { items: paths(removed.filter((node) => !ids.has(node.parentId))), files: paths(removed.filter((node) => node.kind === NodeKind.FILE)) };
}

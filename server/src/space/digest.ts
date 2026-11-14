// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, toBinary } from "@bufbuild/protobuf";
import { type Commit, CommitSchema, type Op, OpSchema } from "../gen/koidrive/v1/commit_pb";
import { type Content, ContentSchema } from "../gen/koidrive/v1/model_pb";
import { sha256 } from "../crypto";

function content(value: Content | undefined): Content | undefined {
  if (!value) return undefined;
  return create(ContentSchema, {
    size: value.size,
    mtimeNs: value.mtimeNs,
    executable: value.executable,
    chunks: value.chunks.map(({ hash, size, encoding }) => ({ hash, size, encoding })),
    chunkCount: value.chunkCount,
    manifestDigest: value.manifestDigest,
    chunker: value.chunker,
  });
}

export type DigestVersion = 1 | 2 | 3;

export type Digests = Record<DigestVersion, Uint8Array>;

function op({ op }: Op, version: DigestVersion): Op {
  switch (op.case) {
    case "createDir": {
      const { nodeId, parentId, name } = op.value;
      const pendingFor = version >= 3 ? op.value.pendingFor : "";
      return create(OpSchema, { op: { case: op.case, value: { nodeId, parentId, name, pendingFor } } });
    }
    case "createFile": {
      const { nodeId, parentId, name } = op.value;
      return create(OpSchema, { op: { case: op.case, value: { nodeId, parentId, name, content: content(op.value.content) } } });
    }
    case "updateContent":
      return create(OpSchema, {
        op: {
          case: op.case,
          value: { nodeId: op.value.nodeId, content: content(op.value.content), resend: version >= 2 && op.value.resend },
        },
      });
    case "move": {
      const { nodeId, newParentId, newName } = op.value;
      const [pendingFor, from] = version >= 3 ? [op.value.pendingFor, op.value.from] : ["", undefined];
      return create(OpSchema, { op: { case: op.case, value: { nodeId, newParentId, newName, pendingFor, from } } });
    }
    case "delete":
      return create(OpSchema, { op: { case: op.case, value: { nodeId: op.value.nodeId } } });
    case "restore": {
      const { nodeId, versionId, targetParentId, name } = op.value;
      return create(OpSchema, { op: { case: op.case, value: { nodeId, versionId, targetParentId, name } } });
    }
    default:
      return create(OpSchema);
  }
}

export async function commitDigest(commit: Commit, version: DigestVersion = 3): Promise<Uint8Array> {
  const projected = create(CommitSchema, {
    baseSeq: commit.baseSeq,
    ops: commit.ops.map((each) => op(each, version)),
    deviceSeq: commit.deviceSeq,
    requireHead: commit.requireHead,
  });
  return sha256(toBinary(CommitSchema, projected, { writeUnknownFields: false }));
}

export async function commitDigests(commit: Commit): Promise<Digests> {
  return { 1: await commitDigest(commit, 1), 2: await commitDigest(commit, 2), 3: await commitDigest(commit, 3) };
}
